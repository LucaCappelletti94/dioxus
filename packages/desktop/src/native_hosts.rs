//! Which native host, a platform window with its webview, renders each logical window.
//!
//! Android replaces and restarts `Activity` hosts while the app runs, so a window can move between hosts.

use std::collections::HashMap;
use std::hash::Hash;

/// What to do when the platform starts a host.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnStarted<H, T> {
    /// Nothing changes.
    Ignore,
    /// Build a new host for `target` on the started native instance, then [`NativeHosts::bind`] it.
    Build { target: T },
    /// Render `target` into the existing detached `host` again, then [`NativeHosts::bind`] it.
    Rebind { target: T, host: H },
}

/// What to do when the platform destroys a host.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnDestroyed<T> {
    /// The host was unknown.
    Ignore,
    /// The host rendered nothing and only its native resources go.
    Released,
    /// The host rendered `target`, which keeps its state and waits for another host.
    Parked(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostState<T> {
    Bound(T),
    Detached,
}

#[derive(Debug, Clone, Copy)]
struct Host<T> {
    state: HostState<T>,
    /// When the host was last started or bound.
    started: u64,
}

#[derive(Debug, Clone, Copy)]
enum Hosting<H> {
    Hosted(H),
    Parked { since: u64 },
}

/// Registry of hosts and the logical windows they render.
#[derive(Debug)]
pub(crate) struct NativeHosts<H, T> {
    hosts: HashMap<H, Host<T>>,
    windows: HashMap<T, Hosting<H>>,
    clock: u64,
}

impl<H, T> Default for NativeHosts<H, T> {
    fn default() -> Self {
        Self {
            hosts: HashMap::new(),
            windows: HashMap::new(),
            clock: 0,
        }
    }
}

impl<H: Copy + Eq + Hash, T: Copy + Eq + Hash> NativeHosts<H, T> {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Make `host` render `target`, returning the previous host, which is now detached.
    pub(crate) fn bind(&mut self, host: H, target: T) -> Option<H> {
        let started = self.tick();
        let previous = match self.windows.insert(target, Hosting::Hosted(host)) {
            Some(Hosting::Hosted(previous)) if previous != host => Some(previous),
            _ => None,
        };
        if let Some(previous) = previous {
            if let Some(previous) = self.hosts.get_mut(&previous) {
                previous.state = HostState::Detached;
            }
        }
        if let Some(Host {
            state: HostState::Bound(other),
            ..
        }) = self.hosts.insert(
            host,
            Host {
                state: HostState::Bound(target),
                started,
            },
        ) {
            if other != target {
                let since = self.tick();
                self.windows.insert(other, Hosting::Parked { since });
            }
        }
        previous
    }

    /// The platform started `host`, either a new native instance or a known one coming back.
    pub(crate) fn started(&mut self, host: H) -> OnStarted<H, T> {
        let now = self.tick();
        let state = match self.hosts.get_mut(&host) {
            Some(known) => {
                known.started = now;
                Some(known.state)
            }
            None => None,
        };
        match state {
            Some(HostState::Bound(_)) => OnStarted::Ignore,
            Some(HostState::Detached) => match self.target_for_started_host() {
                Some(target) => OnStarted::Rebind { target, host },
                None => OnStarted::Ignore,
            },
            None => match self.target_for_started_host() {
                Some(target) => OnStarted::Build { target },
                None => OnStarted::Ignore,
            },
        }
    }

    /// Parked windows take a started host first, oldest first, then the most recently started host's window.
    fn target_for_started_host(&self) -> Option<T> {
        let parked = self
            .windows
            .iter()
            .filter_map(|(target, hosting)| match hosting {
                Hosting::Parked { since } => Some((*since, *target)),
                Hosting::Hosted(_) => None,
            })
            .min_by_key(|(since, _)| *since);
        if let Some((_, target)) = parked {
            return Some(target);
        }
        self.hosts
            .values()
            .filter_map(|host| match host.state {
                HostState::Bound(target) => Some((host.started, target)),
                HostState::Detached => None,
            })
            .max_by_key(|(started, _)| *started)
            .map(|(_, target)| target)
    }

    /// The platform destroyed `host` for good.
    pub(crate) fn destroyed(&mut self, host: H) -> OnDestroyed<T> {
        let Some(removed) = self.hosts.remove(&host) else {
            return OnDestroyed::Ignore;
        };
        match removed.state {
            HostState::Detached => OnDestroyed::Released,
            HostState::Bound(target) => {
                let since = self.tick();
                self.windows.insert(target, Hosting::Parked { since });
                OnDestroyed::Parked(target)
            }
        }
    }

    /// Forget `target` and its host. Detached hosts stay until the platform destroys them.
    pub(crate) fn remove_window(&mut self, target: T) -> Option<H> {
        match self.windows.remove(&target)? {
            Hosting::Hosted(host) => {
                self.hosts.remove(&host);
                Some(host)
            }
            Hosting::Parked { .. } => None,
        }
    }

    /// The logical window that `host` renders, if it renders one.
    pub(crate) fn target_of(&self, host: H) -> Option<T> {
        match self.hosts.get(&host)?.state {
            HostState::Bound(target) => Some(target),
            HostState::Detached => None,
        }
    }

    /// The host rendering `target`, if it has one.
    #[cfg(test)]
    pub(crate) fn host_of(&self, target: T) -> Option<H> {
        match self.windows.get(&target)? {
            Hosting::Hosted(host) => Some(*host),
            Hosting::Parked { .. } => None,
        }
    }

    /// Whether any logical window is still open, hosted or parked.
    pub(crate) fn has_windows(&self) -> bool {
        !self.windows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeHosts, OnDestroyed, OnStarted};

    const T: char = 'T';

    fn live(host: u32) -> NativeHosts<u32, char> {
        let mut hosts = NativeHosts::default();
        assert_eq!(hosts.bind(host, T), None);
        assert_eq!(hosts.started(host), OnStarted::Ignore);
        hosts
    }

    #[test]
    fn destroy_before_create_parks_then_builds_on_the_new_activity() {
        let mut hosts = live(100);
        assert_eq!(hosts.destroyed(100), OnDestroyed::Parked(T));
        assert_eq!(hosts.host_of(T), None);
        assert!(hosts.has_windows(), "a parked window keeps the app open");

        assert_eq!(hosts.started(101), OnStarted::Build { target: T });
        assert_eq!(hosts.bind(101, T), None);
        assert_eq!(hosts.target_of(101), Some(T));
        assert_eq!(hosts.host_of(T), Some(101));
    }

    #[test]
    fn create_before_destroy_moves_the_window_and_releases_only_the_old_host() {
        let mut hosts = live(100);
        assert_eq!(hosts.started(101), OnStarted::Build { target: T });
        assert_eq!(hosts.bind(101, T), Some(100));
        assert_eq!(
            hosts.target_of(100),
            None,
            "the retiring host gets no edits"
        );

        assert_eq!(hosts.destroyed(100), OnDestroyed::Released);
        assert_eq!(hosts.target_of(101), Some(T));
        assert_eq!(hosts.host_of(T), Some(101));
    }

    #[test]
    fn repeated_replacement_in_both_orders_keeps_one_host() {
        let mut hosts = live(100);
        for (old, new, overlap) in [(100, 101, true), (101, 102, false), (102, 103, true)] {
            if overlap {
                assert_eq!(hosts.started(new), OnStarted::Build { target: T });
                assert_eq!(hosts.bind(new, T), Some(old));
                assert_eq!(hosts.destroyed(old), OnDestroyed::Released);
            } else {
                assert_eq!(hosts.destroyed(old), OnDestroyed::Parked(T));
                assert_eq!(hosts.started(new), OnStarted::Build { target: T });
                assert_eq!(hosts.bind(new, T), None);
            }
            assert_eq!(hosts.host_of(T), Some(new));
            assert_eq!(hosts.target_of(old), None);
        }
        assert_eq!(hosts.destroyed(100), OnDestroyed::Ignore);
    }

    #[test]
    fn restarting_the_bound_host_changes_nothing() {
        let mut hosts = live(100);
        assert_eq!(hosts.started(100), OnStarted::Ignore);
        assert_eq!(hosts.host_of(T), Some(100));
    }

    #[test]
    fn a_detached_host_that_starts_again_takes_the_window_back() {
        let mut hosts = live(100);
        assert_eq!(hosts.started(101), OnStarted::Build { target: T });
        assert_eq!(hosts.bind(101, T), Some(100));

        assert_eq!(
            hosts.started(100),
            OnStarted::Rebind {
                target: T,
                host: 100
            }
        );
        assert_eq!(hosts.bind(100, T), Some(101));
        assert_eq!(hosts.target_of(101), None);
        assert_eq!(hosts.destroyed(101), OnDestroyed::Released);
        assert_eq!(hosts.host_of(T), Some(100));
    }

    #[test]
    fn a_parked_window_returns_to_a_detached_host_that_starts() {
        let mut hosts = live(100);
        assert_eq!(hosts.started(101), OnStarted::Build { target: T });
        assert_eq!(hosts.bind(101, T), Some(100));
        assert_eq!(hosts.destroyed(101), OnDestroyed::Parked(T));

        assert_eq!(
            hosts.started(100),
            OnStarted::Rebind {
                target: T,
                host: 100
            }
        );
        assert_eq!(hosts.bind(100, T), None);
        assert_eq!(hosts.host_of(T), Some(100));
    }

    #[test]
    fn a_closed_window_is_not_rebuilt() {
        let mut hosts = live(100);
        assert_eq!(hosts.destroyed(100), OnDestroyed::Parked(T));
        assert_eq!(hosts.remove_window(T), None);
        assert!(!hosts.has_windows());
        assert_eq!(hosts.started(101), OnStarted::Ignore);

        let mut hosts = live(100);
        assert_eq!(hosts.started(101), OnStarted::Build { target: T });
        assert_eq!(hosts.bind(101, T), Some(100));
        assert_eq!(hosts.remove_window(T), Some(101));
        assert_eq!(hosts.target_of(101), None);
        assert_eq!(hosts.destroyed(100), OnDestroyed::Released);
        assert_eq!(hosts.started(102), OnStarted::Ignore);
    }

    #[test]
    fn a_new_host_serves_the_longest_parked_window_first() {
        let mut hosts = NativeHosts::default();
        hosts.bind(100, 'A');
        hosts.bind(101, 'B');
        hosts.bind(102, 'C');
        assert_eq!(hosts.destroyed(101), OnDestroyed::Parked('B'));
        assert_eq!(hosts.destroyed(100), OnDestroyed::Parked('A'));

        assert_eq!(hosts.started(103), OnStarted::Build { target: 'B' });
        hosts.bind(103, 'B');
        assert_eq!(hosts.started(104), OnStarted::Build { target: 'A' });
        hosts.bind(104, 'A');
        assert_eq!(hosts.started(105), OnStarted::Build { target: 'A' });
    }

    #[test]
    fn unknown_hosts_and_an_empty_registry_are_ignored() {
        let mut hosts = NativeHosts::<u32, char>::default();
        assert_eq!(hosts.started(100), OnStarted::Ignore);
        assert_eq!(hosts.destroyed(100), OnDestroyed::Ignore);
        assert!(!hosts.has_windows());
    }
}
