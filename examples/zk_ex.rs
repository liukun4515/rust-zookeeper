extern crate zookeeper;
extern crate log;

use std::{env, io};
use std::time::Duration;
use log::info;
use zookeeper::{WatchedEvent, Watcher, ZooKeeper};

struct LoggingWatcher;

impl Watcher for LoggingWatcher {
    fn handle(&self, e: WatchedEvent) {
        info!("====={:?}", e)
    }
}

fn zk_server_urls() -> String {
    let key = "ZOOKEEPER_SERVERS";
    match env::var(key) {
        Ok(val) => val,
        Err(_) => "localhost:2181".to_string(),
    }
}

fn main() {
    env_logger::init();
    let zk_urls = zk_server_urls();
    println!("connecting to {}", zk_urls);
    let zk = ZooKeeper::connect(&*zk_urls, Duration::from_secs(15), LoggingWatcher).unwrap();

    zk.add_listener(|zk_state| println!("New ZkState is {:?}", zk_state));

    let mut tmp = String::new();
    println!("press enter to exit example");
    io::stdin().read_line(&mut tmp).unwrap();
}