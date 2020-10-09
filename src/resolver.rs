use crate::data::{ResolveResponse, ResultsCache};
use crate::Result;
use futures::StreamExt;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tracing::{error, info, warn};
use trust_dns_resolver::{
    config::LookupIpStrategy, config::NameServerConfigGroup, config::ResolverConfig,
    config::ResolverOpts, error::ResolveError, error::ResolveErrorKind, lookup_ip::LookupIp,
    TokioAsyncResolver,
};

// The maximum number of messages that can be in the channel before calls to .send start waiting
// for the receiver to take from the channel.
const CHANSIZE: usize = 32 * 2;

#[derive(Debug)]
pub struct Resolver {
    config: ResolverConfig,
    options: ResolverOpts,
    nameservers: Vec<IpAddr>,
}

impl Default for Resolver {
    fn default() -> Self {
        let nameservers = vec![
            // Google
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8844)),
            // CloudFlare
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1001)),
        ];

        Resolver {
            config: ResolverConfig::cloudflare(),
            options: ResolverOpts {
                ndots: 1,
                timeout: Duration::from_secs(1),
                attempts: 2,
                rotate: false,
                check_names: true,
                edns0: false,
                validate: false,
                ip_strategy: LookupIpStrategy::Ipv4AndIpv6,
                cache_size: 32,
                use_hosts_file: false,
                positive_min_ttl: None,
                negative_min_ttl: None,
                positive_max_ttl: None,
                negative_max_ttl: None,
                distrust_nx_responses: true,
                num_concurrent_reqs: 2,
                preserve_intermediates: true,
            },
            nameservers,
        }
    }
}

impl Resolver {
    // Sets the timeout
    pub fn timeout(mut self, timeout: u64) -> Self {
        self.options.timeout = Duration::from_secs(timeout);
        self
    }

    // Loads a list of custom resolvers (nameservers) as the resolver config
    pub fn load_resolvers(mut self, path: &str) -> Self {
        let file = std::fs::read_to_string(path).unwrap();
        let ips: Vec<IpAddr> = file.lines().map(|l| l.parse::<IpAddr>().unwrap()).collect();
        let group = NameServerConfigGroup::from_ips_clear(&ips, 53);
        self.config = ResolverConfig::from_parts(None, vec![], group);
        self.nameservers = ips;
        self
    }

    // Handles extracting the records or the errors from the dns query and sends it down the
    // channel. The receiver handles caching the responses before serializing them as json.
    async fn deliver_response(
        mut records_sender: Sender<VecDeque<ResolveResponse>>,
        response: std::result::Result<LookupIp, ResolveError>,
    ) -> Result<()> {
        let mut records: VecDeque<ResolveResponse> = VecDeque::new();
        let mut errors: VecDeque<ResolveResponse> = VecDeque::new();

        match response {
            Ok(r) => {
                // should probably add the query as well
                //let query = r.as_lookup().query().name().to_utf8();
                r.as_lookup()
                    .record_iter()
                    .map(|record| {
                        info!("got {:?}", record);
                        records.push_front(ResolveResponse::from(record));
                    })
                    .for_each(drop);

                records_sender.send(records).await?;
            }

            // All of the errors we care about return `ResolveErrorKind::NoRecordsFound`
            Err(e) => match e.kind() {
                ResolveErrorKind::NoRecordsFound {
                    query,
                    response_code,
                    ..
                } => {
                    warn!("got resolve error for {} {:?}", query, response_code);
                    let query = query.name().to_string();
                    errors.push_front(ResolveResponse::Error {
                        query: query.clone(),
                        response_code: response_code.to_string(),
                    });

                    records_sender.send(errors).await?;
                }
                _ => warn!("got {:?}", e),
            },
        }
        Ok(())
    }

    // Receives the records and adds them into a queue, when the queue is full it's contents will
    // be written into the output file.
    async fn cache_responses(
        mut receiver: Receiver<VecDeque<ResolveResponse>>,
        mut queue_size: usize,
        cache: Arc<ResultsCache>,
        total: usize,
    ) {
        let mut queue_count: usize = 0;

        // If queue size is larger than the total number of targets, set it to the total.
        if queue_size > total {
            queue_size = total
        }

        // Instead of writing to the `ResultsCache` each time we receieve a response, we only
        // write when the queue contains `queue_size` number of responses. This is a neat little
        // optimisation that will stop us from asking for the lock for every response we get.
        let mut queue: VecDeque<ResolveResponse> = VecDeque::with_capacity(queue_size);
        while let Some(mut records) = receiver.recv().await {
            info!("added {} responses to the queue", records.len());
            queue_count += records.len();
            queue.append(&mut records);

            // Queue is full, write results into the cache
            if queue_count == queue_size {
                info!("queue is full, writing {} responses to cache", queue.len());
                let cache = Arc::clone(&cache);
                cache.insert(&mut queue).await;
                queue_count = 0;
            }
        }

        // If there is anything remaining in the queue than write it to the output file.
        if !queue.is_empty() {
            info!("caching the remaining contents of the queue");
            let cache = Arc::clone(&cache);
            cache.insert(&mut queue).await;
        }
    }

    // Create a resolver for each name server, and then spawn a task for each one. This is required
    // because we want to retrieve the record even if two nameservers results conflict with each other. If
    // we didn't care about retrieving conflicting records, we could just make one
    // `TokioAsyncResolver` with a `NameServerConfigGroup` containing all the nameservers
    async fn enumerate_ns(
        &self,
        target: String,
        sender: Sender<std::result::Result<LookupIp, ResolveError>>,
    ) {
        // instead of sending a single LookupIp across the channel each time, maybe we should
        // instead send them in batches of Vec<LookupIp, ResolveError> ?
        let resolvers = self.nameservers.clone();
        let tx = sender.clone();
        let results = futures::stream::iter(resolvers)
            .map(|ns| {
                let t = target.clone();
                let mut tx = tx.clone();
                let group = NameServerConfigGroup::from_ips_clear(&[ns], 53);
                let resolver = TokioAsyncResolver::tokio(
                    ResolverConfig::from_parts(None, vec![], group),
                    self.options,
                )
                .expect("error building resolver");
                tokio::spawn(async move {
                    // Cheaper query
                    // https://docs.rs/trust-dns-resolver/0.20.0-alpha.2/trust_dns_resolver/struct.AsyncResolver.html#method.lookup_ip
                    let resp = resolver.lookup_ip(t + ".").await;
                    tx.send(resp).await
                })
            })
            .buffer_unordered(32) // 32 nameservers at once
            .collect::<Vec<_>>();
        results.await;
    }

    // Does dual stack IPV4 & IPV6 lookups and writes the results to a file on disk
    pub async fn resolve(
        self,
        hosts: Vec<String>,
        concurrency: usize,
        path: PathBuf,
    ) -> Result<()> {
        use tokio::prelude::*;
        let mut outfile = fs::File::create(&path).await?;
        let total = hosts.len() * self.nameservers.len();
        let cache = ResultsCache::new();
        let resolver = Arc::new(self);
        let queue_size: usize = 256;

        let (lookup_sender, mut lookup_receiver) =
            channel::<std::result::Result<LookupIp, ResolveError>>(CHANSIZE);
        let (records_sender, records_receiver) = channel::<VecDeque<ResolveResponse>>(CHANSIZE);

        // Handles storing the itermediate results before writing the final output to disk.
        let results_map2 = Arc::clone(&cache);
        let output_manager = tokio::spawn(async move {
            Resolver::cache_responses(records_receiver, queue_size, results_map2, total).await
        });

        // Recieves the responses and fires off a task to convert the `LookupIp` into our `Record`
        // type and deliver it to the channel that will insert it into the `ResultsCache`
        let response_manager = tokio::spawn(async move {
            while let Some(response) = lookup_receiver.recv().await {
                let records_sender = records_sender.clone();
                // Push the handling of the responses off into their own tasks.
                tokio::spawn(
                    async move { Resolver::deliver_response(records_sender, response).await },
                );
            }
        });

        // Iterate over each of the hosts and spawn a new task for each dns lookup
        let producer = futures::stream::iter(hosts)
            .map(|host| {
                let resolver = Arc::clone(&resolver);
                let lookup_sender = lookup_sender.clone();
                tokio::spawn(async move { resolver.enumerate_ns(host, lookup_sender).await })
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>();

        producer.await;
        drop(lookup_sender);
        response_manager.await?;
        output_manager.await?;

        let json = cache.json().await;
        match outfile.write_all(&json.as_bytes()).await {
            Ok(_) => info!("wrote {} bytes to the output file", json.len()),
            Err(e) => error!("couldn't write to output file; got error {}", e),
        }
        println!(
            "Done! {} records written to {:?}",
            cache.num_results().await,
            path
        );

        Ok(())
    }
}