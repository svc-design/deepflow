/*
 * Copyright (c) 2024 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use arc_swap::access::Access;
use log::{debug, info, warn};
#[cfg(target_os = "windows")]
use sysinfo::NetworkExt;
use sysinfo::{get_current_pid, Pid, ProcessExt, ProcessRefreshKind, System, SystemExt};

#[cfg(target_os = "linux")]
use crate::utils::{cgroups, environment::SocketInfo};

use crate::{
    config::handler::EnvironmentAccess,
    error::{Error, Result},
    utils::{
        environment::get_disk_usage,
        process::{get_current_sys_memory_percentage, get_file_and_size_sum},
        stats::{
            self, Collector, Countable, Counter, CounterType, CounterValue, RefCountable,
            StatsOption,
        },
    },
};

#[cfg(target_os = "linux")]
use public::netns::{self, NsFile};
use public::utils::net::{link_list, Link};

#[derive(Default)]
struct NetMetricArg {
    pub rx: u64,
    pub tx: u64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub drop_in: u64,
    pub drop_out: u64,
}

#[derive(Default)]
struct NetMetric {
    rx: AtomicU64,
    tx: AtomicU64,
    tx_bytes: AtomicU64,
    rx_bytes: AtomicU64,
    drop_in: AtomicU64,
    drop_out: AtomicU64,
}

struct LinkStatusBroker {
    running: AtomicBool,
    old: NetMetric,
    new: NetMetric,
}

impl LinkStatusBroker {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            old: NetMetric::default(),
            new: NetMetric::default(),
        }
    }

    pub fn close(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn closed(&self) -> bool {
        !self.running.load(Ordering::Relaxed)
    }

    pub fn update(&self, new_metric: NetMetricArg) {
        let NetMetricArg {
            rx,
            tx,
            tx_bytes,
            rx_bytes,
            drop_in,
            drop_out,
        } = new_metric;
        self.new.rx.store(rx, Ordering::Relaxed);
        self.new.tx.store(tx, Ordering::Relaxed);
        self.new.rx_bytes.store(rx_bytes, Ordering::Relaxed);
        self.new.tx_bytes.store(tx_bytes, Ordering::Relaxed);
        self.new.drop_in.store(drop_in, Ordering::Relaxed);
        self.new.drop_out.store(drop_out, Ordering::Relaxed);
    }
}

impl RefCountable for LinkStatusBroker {
    fn get_counters(&self) -> Vec<Counter> {
        if !self.running.load(Ordering::SeqCst) {
            return vec![];
        }

        let mut metrics = vec![];
        let new_rx = self.new.rx.load(Ordering::Relaxed);
        let old_rx = self.old.rx.swap(new_rx, Ordering::Relaxed);
        metrics.push((
            "rx",
            CounterType::Counted,
            CounterValue::Unsigned(new_rx.overflowing_sub(old_rx).0),
        ));
        let new_tx = self.new.tx.load(Ordering::Relaxed);
        let old_tx = self.old.tx.swap(new_tx, Ordering::Relaxed);
        metrics.push((
            "tx",
            CounterType::Counted,
            CounterValue::Unsigned(new_tx.overflowing_sub(old_tx).0),
        ));
        let new_tx_bytes = self.new.tx_bytes.load(Ordering::Relaxed);
        let old_tx_bytes = self.old.tx_bytes.swap(new_tx_bytes, Ordering::Relaxed);
        metrics.push((
            "tx_bytes",
            CounterType::Counted,
            CounterValue::Unsigned(new_tx_bytes.overflowing_sub(old_tx_bytes).0),
        ));
        let new_rx_bytes = self.new.rx_bytes.load(Ordering::Relaxed);
        let old_rx_bytes = self.old.rx_bytes.swap(new_rx_bytes, Ordering::Relaxed);
        metrics.push((
            "rx_bytes",
            CounterType::Counted,
            CounterValue::Unsigned(new_rx_bytes.overflowing_sub(old_rx_bytes).0),
        ));
        let new_drop_in = self.new.drop_in.load(Ordering::Relaxed);
        let old_drop_in = self.old.drop_in.swap(new_drop_in, Ordering::Relaxed);
        metrics.push((
            "drop_in",
            CounterType::Counted,
            CounterValue::Unsigned(new_drop_in.overflowing_sub(old_drop_in).0),
        ));
        let new_drop_out = self.new.drop_out.load(Ordering::Relaxed);
        let old_drop_out = self.old.drop_out.swap(new_drop_out, Ordering::Relaxed);
        metrics.push((
            "drop_out",
            CounterType::Counted,
            CounterValue::Unsigned(new_drop_out.overflowing_sub(old_drop_out).0),
        ));

        metrics
    }
}

struct SysStatusBroker {
    system: Arc<Mutex<System>>,
    pid: Pid,
    create_time: Duration,
    log_dir: String,
    config: EnvironmentAccess,
}

impl SysStatusBroker {
    pub fn new(
        system: Arc<Mutex<System>>,
        log_dir: String,
        config: EnvironmentAccess,
    ) -> Result<Self> {
        let pid = get_current_pid().map_err(|e| Error::SysMonitor(String::from(e)))?;

        let create_time = {
            let mut system_guard = system.lock().unwrap();
            if !system_guard.refresh_process_specifics(pid, ProcessRefreshKind::new().with_cpu()) {
                return Err(Error::SysMonitor(format!(
                    "couldn't refresh process with pid({})",
                    pid
                )));
            }
            system_guard
                .process(pid)
                .map(|p| Duration::from_secs(p.start_time()))
                .ok_or(Error::SysMonitor(format!(
                    "couldn't get process start time with pid({})",
                    pid
                )))?
        };
        Ok(Self {
            system,
            pid,
            create_time,
            log_dir,
            config,
        })
    }
}

impl RefCountable for SysStatusBroker {
    fn get_counters(&self) -> Vec<Counter> {
        let mut system_guard = self.system.lock().unwrap();
        // 只有在进程不存在的时候会返回false，基本不会报错
        if !system_guard.refresh_process_specifics(self.pid, ProcessRefreshKind::new().with_cpu()) {
            warn!("refresh process failed, system status monitor has stopped");
            return vec![];
        }

        let mut metrics = vec![];
        let (current_sys_free_memory_percentage, current_sys_available_memory_percentage) =
            get_current_sys_memory_percentage();
        metrics.push((
            "sys_free_memory",
            CounterType::Gauged,
            CounterValue::Unsigned(current_sys_free_memory_percentage as u64),
        ));
        metrics.push((
            "sys_available_memory",
            CounterType::Gauged,
            CounterValue::Unsigned(current_sys_available_memory_percentage as u64),
        ));

        let config = self.config.load();
        let sys_memory_limit = config.sys_memory_limit as f64;

        let (sys_free_memory_limit_ratio, sys_available_memory_limit_ratio) =
            if sys_memory_limit > 0.0 {
                (
                    current_sys_free_memory_percentage as f64 / sys_memory_limit,
                    current_sys_available_memory_percentage as f64 / sys_memory_limit,
                )
            } else {
                (0.0, 0.0) // If sys_memory_limit is set to 0, it means that there is no need to check if the system's free/available memory is too low. In this case, 0.0 will be directly returned, indicating that there will be no low system free/available memory alert.
            };
        metrics.push((
            "sys_free_memory_limit_ratio",
            CounterType::Gauged,
            CounterValue::Float(sys_free_memory_limit_ratio),
        ));
        metrics.push((
            "sys_available_memory_limit_ratio",
            CounterType::Gauged,
            CounterValue::Float(sys_available_memory_limit_ratio),
        ));

        match get_file_and_size_sum(&self.log_dir) {
            Ok(file_and_size_sum) => {
                metrics.push((
                    "log_file_size_sum",
                    CounterType::Gauged,
                    CounterValue::Unsigned(file_and_size_sum.file_sizes_sum),
                ));
                metrics.push((
                    "log_file_amount",
                    CounterType::Gauged,
                    CounterValue::Unsigned(file_and_size_sum.file_infos.len() as u64),
                ));
            }
            Err(e) => {
                warn!("get file and size sum failed: {:?}", e);
            }
        }

        match system_guard.process(self.pid) {
            Some(process) => {
                let cpu_usage = process.cpu_usage() as f64;
                let mem_used = process.memory(); // in bytes

                metrics.push((
                    "cpu_percent",
                    CounterType::Gauged,
                    CounterValue::Float(cpu_usage),
                ));
                metrics.push((
                    "max_millicpus_ratio",
                    CounterType::Gauged,
                    CounterValue::Float(cpu_usage * 10.0 / config.max_millicpus as f64),
                ));
                metrics.push((
                    "memory",
                    CounterType::Gauged,
                    CounterValue::Unsigned(mem_used),
                ));
                metrics.push((
                    "max_memory_ratio",
                    CounterType::Gauged,
                    CounterValue::Float(mem_used as f64 / config.max_memory as f64),
                ));
                metrics.push((
                    "create_time",
                    CounterType::Gauged,
                    CounterValue::Unsigned(self.create_time.as_millis() as u64),
                ));
            }
            None => {
                warn!("get process data failed, system status monitor has stopped");
            }
        }

        #[cfg(target_os = "linux")]
        metrics.push((
            "open_sockets",
            CounterType::Gauged,
            match SocketInfo::get() {
                Ok(SocketInfo {
                    tcp,
                    tcp6,
                    udp,
                    udp6,
                }) => {
                    CounterValue::Unsigned((tcp.len() + tcp6.len() + udp.len() + udp6.len()) as u64)
                }
                Err(_) => CounterValue::Unsigned(0),
            },
        ));
        #[cfg(target_os = "linux")]
        metrics.push((
            "page_cache",
            CounterType::Gauged,
            if let Some(m_stat) = cgroups::memory_info() {
                CounterValue::Unsigned(m_stat.stat.cache)
            } else {
                CounterValue::Unsigned(0)
            },
        ));
        metrics
    }
}

struct SysLoad(Arc<Mutex<System>>);

impl RefCountable for SysLoad {
    fn get_counters(&self) -> Vec<Counter> {
        let mut sys = self.0.lock().unwrap();
        sys.refresh_cpu();
        vec![
            (
                "load1",
                CounterType::Gauged,
                CounterValue::Float(sys.load_average().one),
            ),
            (
                "load5",
                CounterType::Gauged,
                CounterValue::Float(sys.load_average().five),
            ),
            (
                "load15",
                CounterType::Gauged,
                CounterValue::Float(sys.load_average().fifteen),
            ),
        ]
    }
}

struct NetStats<'a>(&'a Link);

impl stats::Module for NetStats<'_> {
    fn name(&self) -> &'static str {
        "net"
    }

    fn tags(&self) -> Vec<StatsOption> {
        vec![
            StatsOption::Tag("name", self.0.name.clone()),
            StatsOption::Tag("mac", self.0.mac_addr.to_string()),
        ]
    }
}

struct FreeDiskUsage {
    directory: String,
}

impl stats::Module for FreeDiskUsage {
    fn name(&self) -> &'static str {
        "free_disk"
    }

    fn tags(&self) -> Vec<StatsOption> {
        vec![StatsOption::Tag("directory", self.directory.clone())]
    }
}

impl RefCountable for FreeDiskUsage {
    fn get_counters(&self) -> Vec<Counter> {
        let mut metrics = vec![];
        match get_disk_usage(&self.directory) {
            Ok((total, free)) => {
                metrics.push((
                    "free_disk_percentage",
                    CounterType::Gauged,
                    CounterValue::Float(free as f64 * 100.0 / total as f64),
                ));
                metrics.push((
                    "free_disk_absolute",
                    CounterType::Gauged,
                    CounterValue::Unsigned(free as u64),
                ));
            }
            Err(e) => {
                warn!("get disk free usage failed: {:?}", e);
            }
        }
        metrics
    }
}

pub struct Monitor {
    stats: Arc<Collector>,
    running: AtomicBool,
    sys_monitor: Arc<SysStatusBroker>,
    sys_load: Arc<SysLoad>,
    link_map: Arc<Mutex<HashMap<String, Arc<LinkStatusBroker>>>>,
    system: Arc<Mutex<System>>,
    config: EnvironmentAccess,
    free_disks_config: Arc<Mutex<Vec<String>>>,
    free_disk_counters: Arc<Mutex<Vec<Arc<FreeDiskUsage>>>>,
}

impl Monitor {
    pub fn new(stats: Arc<Collector>, log_dir: String, config: EnvironmentAccess) -> Result<Self> {
        let mut system = System::new();
        system.refresh_cpu();
        let system = Arc::new(Mutex::new(system));

        Ok(Self {
            stats,
            running: AtomicBool::new(false),
            sys_monitor: Arc::new(SysStatusBroker::new(
                system.clone(),
                log_dir,
                config.clone(),
            )?),
            sys_load: Arc::new(SysLoad(system.clone())),
            link_map: Arc::new(Mutex::new(HashMap::new())),
            system,
            config: config.clone(),
            free_disks_config: Arc::new(Mutex::new(vec![])),
            free_disk_counters: Arc::new(Mutex::new(vec![])),
        })
    }

    pub fn start(&self) {
        if self.running.swap(true, Ordering::Relaxed) {
            debug!("monitor has already started");
            return;
        }

        // register network hook
        let stats = self.stats.clone();
        #[cfg(target_os = "windows")]
        let system = self.system.clone();
        let link_map = self.link_map.clone();
        self.stats.register_pre_hook(Box::new(move || {
            let mut link_map_guard = link_map.lock().unwrap();

            #[cfg(target_os = "linux")]
            if let Err(e) = NsFile::Root.open_and_setns() {
                warn!("agent must have CAP_SYS_ADMIN to run without 'hostNetwork: true'.");
                warn!("setns error: {}", e);
                return;
            }

            // resolve network interface update
            let links = match link_list() {
                Ok(links) => links,
                Err(e) => {
                    warn!("get interface list error: {}", e);
                    #[cfg(target_os = "linux")]
                    if let Err(e) = netns::reset_netns() {
                        warn!("reset netns error: {}", e);
                    };
                    return;
                }
            };

            let mut del_monitor_list = vec![];
            link_map_guard.retain(|name, broker| {
                let exist = links.iter().any(|link| link.name == name.as_str());
                if !exist {
                    // 通知 stats模块Collector关闭对应broker
                    broker.close();
                }
                let is_retain = exist && !broker.closed();
                if !is_retain {
                    del_monitor_list.push(name.clone());
                }
                is_retain
            });
            if !del_monitor_list.is_empty() {
                debug!("removing monitor interface list: {:?}", del_monitor_list);
            }

            let mut monitor_list = vec![];
            for link in links.iter() {
                if link_map_guard.contains_key(&link.name) {
                    continue;
                }
                let link_broker = Arc::new(LinkStatusBroker::new());
                stats.register_countable(
                    &NetStats(&link),
                    Countable::Ref(Arc::downgrade(&link_broker) as Weak<dyn RefCountable>),
                );
                link_map_guard.insert(link.name.clone(), link_broker);
                monitor_list.push(link.name.clone());
            }

            if !monitor_list.is_empty() {
                debug!("adding new monitor interface list: {:?}", monitor_list);
            }

            #[cfg(any(target_os = "linux", target_os = "android"))]
            for link in links {
                if let Some(broker) = link_map_guard.get(&link.name) {
                    broker.update(NetMetricArg {
                        rx: link.stats.rx_packets,
                        tx: link.stats.tx_packets,
                        rx_bytes: link.stats.rx_bytes,
                        tx_bytes: link.stats.tx_bytes,
                        drop_in: link.stats.rx_dropped,
                        drop_out: link.stats.tx_dropped,
                    });
                }
            }

            #[cfg(target_os = "windows")]
            {
                let mut system_guard = system.lock().unwrap();
                system_guard.refresh_networks_list();
                for (interface, net_data) in system_guard.networks() {
                    if let Some(broker) = link_map_guard.get(interface) {
                        broker.update(NetMetricArg {
                            rx: net_data.total_packets_received(),
                            tx: net_data.total_packets_transmitted(),
                            rx_bytes: net_data.total_received(),
                            tx_bytes: net_data.total_transmitted(),
                            drop_in: net_data.total_errors_on_received(),
                            drop_out: net_data.total_errors_on_transmitted(),
                        });
                    }
                }
            }

            #[cfg(target_os = "linux")]
            if let Err(e) = netns::reset_netns() {
                warn!("reset netns error: {}", e);
            };
        }));

        self.stats.register_countable(
            &stats::NoTagModule("monitor"),
            Countable::Ref(Arc::downgrade(&self.sys_monitor) as Weak<dyn RefCountable>),
        );

        self.stats.register_countable(
            &stats::NoTagModule("system"),
            Countable::Ref(Arc::downgrade(&self.sys_load) as Weak<dyn RefCountable>),
        );

        let config = self.config.clone();
        let stats_collector = self.stats.clone();
        let free_disks_config = self.free_disks_config.clone();
        let free_disk_counters = self.free_disk_counters.clone();
        self.stats.register_pre_hook(Box::new(move || {
            let config_load = config.load();
            let mut free_disks_config = free_disks_config.lock().unwrap();
            if config_load.free_disk_circuit_breaker_directories == *free_disks_config {
                return;
            }

            let mut locked_counters = free_disk_counters.lock().unwrap();
            let old_data = std::mem::take(&mut *locked_counters);
            stats_collector
                .deregister_countables(old_data.iter().map(|c| c.as_ref() as &dyn stats::Module));

            for free_disk in &config_load.free_disk_circuit_breaker_directories {
                let free_disk_counter = Arc::new(FreeDiskUsage {
                    directory: free_disk.clone(),
                });
                stats_collector.register_countable(
                    &FreeDiskUsage {
                        directory: free_disk.clone(),
                    },
                    Countable::Ref(Arc::downgrade(&free_disk_counter) as Weak<dyn RefCountable>),
                );
                locked_counters.push(free_disk_counter);
            }

            info!(
                "update free disk monitor from {:?} to {:?}",
                free_disks_config, config_load.free_disk_circuit_breaker_directories
            );
            *free_disks_config = config_load.free_disk_circuit_breaker_directories.clone();
        }));

        info!("monitor started");
    }

    pub fn stop(&self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            debug!("monitor has already stopped");
            return;
        }
        // tear down
        self.link_map
            .lock()
            .unwrap()
            .drain()
            .for_each(|(_, broker)| broker.close());
        info!("monitor stopped");
    }
}
