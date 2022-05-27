use crate::message::{Message, Put, Get};
use crate::actor::{Actor, ActorContext, Addr};
use crate::{Config};
use crate::utils::{BoundedHashSet, BoundedHashMap};
use crate::adapters::{SledStorage, MemoryStorage, WsServer, OutgoingWebsocketManager, Multicast};
use std::sync::atomic::{AtomicUsize, Ordering};
//use std::time::Instant;
//use sysinfo::{ProcessorExt, System, SystemExt};
//use tokio::time::{sleep, Duration};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use log::{debug, error, info};
use rand::{seq::IteratorRandom, thread_rng};

static SEEN_MSGS_MAX_SIZE: usize = 10000;

struct SeenGetMessage {
    from: Addr,
    last_reply_checksum: Option<i32>,
}

pub struct Router {
    config: Config,
    known_peers: HashSet<Addr>, // ping them periodically to remove closed addrs? and sort by timestamp & prefer long-lasting conns
    storage_adapters: HashSet<Addr>,
    network_adapters: HashSet<Addr>,
    server_peers: HashSet<Addr>, // temporary, so we can forward stuff to outgoing websocket peers (servers)
    seen_messages: BoundedHashSet,
    seen_get_messages: BoundedHashMap<String, SeenGetMessage>,
    subscribers_by_topic: HashMap<String, HashSet<Addr>>,
    msg_counter: AtomicUsize,
}

#[async_trait]
impl Actor for Router {
    /// Listen to incoming messages and start [Actor]s
    async fn pre_start(&mut self, ctx: &ActorContext) {
        let config = &self.config;
        if config.multicast {
            let addr = ctx.start_actor(Box::new(Multicast::new(config.clone())));
            self.server_peers.insert(addr);
        }
        if config.websocket_server {
            let addr = ctx.start_actor(Box::new(WsServer::new(config.clone())));
            self.network_adapters.insert(addr);
        }
        if config.sled_storage {
            let addr = ctx.start_actor(Box::new(SledStorage::new(config.clone())));
            self.storage_adapters.insert(addr);
        }
        if config.memory_storage {
            let addr = ctx.start_actor(Box::new(MemoryStorage::new()));
            self.storage_adapters.insert(addr);
        }
        if config.outgoing_websocket_peers.len() > 0 {
            let actor = OutgoingWebsocketManager::new(config.clone());
            let addr = ctx.start_actor(Box::new(actor));
            self.server_peers.insert(addr.clone());
            self.network_adapters.insert(addr);
        }

        if self.config.stats {
            self.update_stats();
        }
    }

    async fn stopping(&mut self, _ctx: &ActorContext) {
        info!("Router stopping");
    }

    async fn handle(&mut self, msg: Message, _ctx: &ActorContext) {
        debug!("incoming message {}", msg.clone().to_string());
        match msg {
            Message::Put(put) => self.handle_put(put),
            Message::Get(get) => self.handle_get(get),
            Message::Hi { from, peer_id: _ } => { self.known_peers.insert(from); }
        };
    }
}

impl Router {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            known_peers: HashSet::new(),
            storage_adapters: HashSet::new(),
            network_adapters: HashSet::new(),
            server_peers: HashSet::new(),
            seen_messages: BoundedHashSet::new(SEEN_MSGS_MAX_SIZE),
            seen_get_messages: BoundedHashMap::new(SEEN_MSGS_MAX_SIZE),
            subscribers_by_topic: HashMap::new(),
            msg_counter: AtomicUsize::new(0),
        }
    }

    fn update_stats(&self) {
        /*
        let mut stats = node.get("node_stats").get(&peer_id);
        let start_time = Instant::now();
        let msg_counter = self.msg_counter;
        tokio::task::spawn(async move {
            let mut sys = System::new_all();
            loop { // TODO break
                sys.refresh_all();
                stats.get("msgs_per_second").put(msg_counter.load(Ordering::Relaxed).into());
                msg_counter.store(0, Ordering::Relaxed);
                stats.get("total_memory").put(format!("{} MB", sys.total_memory() / 1000).into());
                stats.get("used_memory").put(format!("{} MB", sys.used_memory() / 1000).into());
                stats.get("cpu_usage").put(format!("{} %", sys.global_processor_info().cpu_usage() as u64).into());
                let uptime_secs = start_time.elapsed().as_secs();
                let uptime;
                if uptime_secs <= 60 {
                    uptime = format!("{} seconds", uptime_secs);
                } else if uptime_secs <= 2 * 60 * 60 {
                    uptime = format!("{} minutes", uptime_secs / 60);
                } else {
                    uptime = format!("{} hours", uptime_secs / 60 / 60);
                }
                stats.get("process_uptime").put(uptime.into());
                sleep(Duration::from_millis(1000)).await;
            }
        });
         */
    }

    // record subscription & relay
    fn handle_get(&mut self, get: Get) {
        if !get.id.chars().all(char::is_alphanumeric) {
            error!("id {}", get.id);
        }
        if self.is_message_seen(&get.id) {
            return;
        }
        let seen_get_message = SeenGetMessage { from: get.from.clone(), last_reply_checksum: None };
        self.seen_get_messages.insert(get.id.clone(), seen_get_message);

        // Record subscriber
        let topic = get.node_id.split("/").next().unwrap_or("");
        debug!("{} subscribed to {}", get.from, topic);
        self.subscribers_by_topic.entry(topic.to_string())
            .or_insert_with(HashSet::new).insert(get.from.clone());

        // Ask storage
        for addr in self.storage_adapters.iter() {
            let _ = addr.sender.send(Message::Get(get.clone()));
        }

        // Send to server peers
        for addr in self.server_peers.iter() {
            debug!("send to server peer");
            let _ = addr.sender.send(Message::Get(get.clone()));
        }

        // Ask network
        let mut errored = HashSet::new();
        let mut sent_to = 0;
        let mut rng = thread_rng();
        if let Some(topic_subscribers) = self.subscribers_by_topic.get(topic) {
            // should have a list of all peers and send to those who are the likeliest to respond (MANET)
            let sample = topic_subscribers.iter().choose_multiple(&mut rng, 4);
            for addr in sample {
                if get.from == *addr {
                    continue;
                }
                match addr.sender.send(Message::Get(get.clone())) {
                    Ok(_) => { sent_to += 1; },
                    _=> { errored.insert(addr.clone()); }
                }
            }
        }
        debug!("sent get to a random sample of subscribers of size {}", sent_to);
        if errored.len() > 0 {
            if let Some(topic_subscribers) = self.subscribers_by_topic.get_mut(topic) {
                for addr in errored {
                    topic_subscribers.remove(&addr);
                    self.known_peers.remove(&addr);
                }
            }
        }
        if sent_to < 1 {
            let mut errored = HashSet::new();
            while let Some(addr) = self.known_peers.iter().choose(&mut rng) {
                match addr.sender.send(Message::Get(get.clone())) {
                    Ok(_) => { break },
                    _=> { errored.insert(addr.clone()); }
                }
            }
            for addr in errored {
                self.known_peers.remove(&addr);
            }
        }
    }

    // relay to original requester or all subscribers
    fn handle_put(&mut self, put: Put) {
        if self.is_message_seen(&put.id) {
            return;
        }

        match &put.in_response_to {
            Some(in_response_to) => {
                if let Some(seen_get_message) = self.seen_get_messages.get_mut(in_response_to) {
                    if put.checksum != None && put.checksum == seen_get_message.last_reply_checksum {
                        debug!("same reply already sent");
                        return;
                    } // failing these conditions, should we still send the ack to someone?
                    seen_get_message.last_reply_checksum = put.checksum.clone();
                    let _ = seen_get_message.from.sender.send(Message::Put(put));
                }
            },
            _ => {
                // Save to storage
                for addr in self.storage_adapters.iter() {
                    if put.from == *addr {
                        continue;
                    }
                    // TODO send Gets to... someone, not everyone
                    let _ = addr.sender.send(Message::Put(put.clone()));
                }

                // Send to server peers
                for addr in self.server_peers.iter() {
                    let _ = addr.sender.send(Message::Put(put.clone()));
                }

                // Relay to subscribers
                let mut already_sent_to = HashSet::new();
                let mut sent_to = 0;
                for node_id in put.clone().updated_nodes.keys() {
                    let topic = node_id.split("/").next().unwrap_or("");
                    if let Some(topic_subscribers) = self.subscribers_by_topic.get_mut(topic) {
                        topic_subscribers.retain(|addr| {  // send & remove closed addresses
                            if put.from == *addr {
                                return true;
                            }
                            if already_sent_to.contains(addr) {
                                return true;
                            }
                            already_sent_to.insert(addr.clone());
                            match addr.sender.send(Message::Put(put.clone())) {
                                Ok(_) => { sent_to += 1; true },
                                _=> { false }
                            }
                        })
                    }
                }
                if sent_to < 1 {
                    let mut rng = thread_rng();
                    let mut errored = HashSet::new();
                    while let Some(addr) = self.known_peers.iter().choose(&mut rng) {
                        match addr.sender.send(Message::Put(put.clone())) {
                            Ok(_) => { break },
                            _=> { errored.insert(addr.clone()); }
                        }
                    }
                    for addr in errored {
                        self.known_peers.remove(&addr);
                    }
                }
            }
        };
    }

    fn is_message_seen(&mut self, id: &String) -> bool {
        self.msg_counter.fetch_add(1, Ordering::Relaxed);

        if self.seen_messages.contains(id) {
            debug!("already seen message {}", id);
            return true;
        }
        self.seen_messages.insert(id.clone());

        return false;
    }
}