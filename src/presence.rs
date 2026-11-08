//! Whether the user is at the desktop, from logind's session on the system
//! bus, and each resume from sleep: `docs/app/nearby.md#at-the-desktop`.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream};

use crate::bluez::Backoff;

/// How long one call to logind, or to the bus, may take: a guess, as
/// `volume::CALL_LIMIT` is.
pub const CALL_LIMIT: Duration = Duration::from_secs(1);

const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
/// See `docs/protocol/linux/desktop.md#logind`.
const AUTO_SESSION: &str = "/org/freedesktop/login1/session/auto";
const SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";

/// What logind has told, for the nearby notifier, in one value so that
/// whoever reads it gets both as of the same signal
/// (`docs/app/nearby.md#at-the-desktop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// Whether the user is at the desktop: true wherever logind cannot tell.
    pub at_desktop: bool,
    pub resumes: Resumes,
}

impl Default for Presence {
    /// At the desktop, and no resume yet.
    fn default() -> Self {
        Self {
            at_desktop: true,
            resumes: Resumes::default(),
        }
    }
}

/// The resumes from sleep taken in so far (`docs/app/nearby.md#a-return`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resumes {
    /// How many: a count, so that latest-wins loses none.
    pub count: u64,
    /// When the last sleep began, on the wall clock: when its
    /// `PrepareForSleep(true)` came, or, with none before the resume, when
    /// the resume came. `None` before the first resume.
    pub slept: Option<SystemTime>,
}

/// The session's three properties the answer depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    active: bool,
    idle: bool,
    locked: bool,
}

impl Session {
    /// In the foreground, neither idle nor locked.
    fn at_desktop(self) -> bool {
        self.active && !self.idle && !self.locked
    }

    /// Takes in what a `GetAll` or a `PropertiesChanged` carries; a property
    /// it leaves out, or carries as no boolean, keeps its value.
    fn apply(&mut self, properties: &HashMap<String, OwnedValue>) {
        let read = |name: &str, value: &mut bool| {
            if let Some(v) = properties.get(name).and_then(|v| bool::try_from(v).ok()) {
                *value = v;
            }
        };
        read("Active", &mut self.active);
        read("IdleHint", &mut self.idle);
        read("LockedHint", &mut self.locked);
    }
}

impl Default for Session {
    /// At the desktop: what a property never read counts as.
    fn default() -> Self {
        Self {
            active: true,
            idle: false,
            locked: false,
        }
    }
}

/// Keeps `presence` up with the user's session on the system bus, and
/// takes each resume from sleep into it, for as long as the app runs:
/// set up again on [`Backoff`] whenever the bus cannot be reached, logind
/// does not answer, or its signals end. True whenever logind cannot tell.
pub async fn run(presence: watch::Sender<Presence>) {
    let mut backoff = Backoff::default();
    // A failure that stays - no session of the user's, say - warns once.
    let mut warned = false;
    loop {
        let began = Instant::now();
        let watched = async {
            let conn = bounded("connecting to the system bus", async {
                Ok(zbus::connection::Builder::system()?.build().await?)
            })
            .await?;
            watch(&conn, &presence).await
        }
        .await;
        // Until logind tells again, at the desktop.
        presence.send_if_modified(|p| !std::mem::replace(&mut p.at_desktop, true));
        let wait = match watched {
            Ok(()) => {
                warned = false;
                backoff.ended(began.elapsed())
            }
            Err(e) => {
                if std::mem::replace(&mut warned, true) {
                    tracing::debug!("cannot tell whether the user is at the desktop: {e:#}");
                } else {
                    tracing::warn!("cannot tell whether the user is at the desktop: {e:#}");
                }
                backoff.failed()
            }
        };
        tracing::debug!("following the login session again in {}s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// Follows the session on `conn` into `presence`, and takes each resume
/// from sleep into it, in the order logind sent them, until logind's signals end or its name changes
/// hands. With logind not on the bus, sets true and waits for it. With no
/// session found, sets true, goes on taking resumes in, and looks for the
/// session again on [`Backoff`].
pub async fn watch(conn: &Connection, presence: &watch::Sender<Presence>) -> Result<()> {
    let set = |at: bool| {
        presence.send_if_modified(|p| std::mem::replace(&mut p.at_desktop, at) != at);
    };
    // Subscribed before anything is read, so that no change falls between.
    let owners = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, LOGIN1)?
        .build();
    let owners = bounded("subscribing to logind's name", async {
        Ok(MessageStream::for_match_rule(owners, conn, None).await?)
    })
    .await?;
    // Every signal of logind's in one stream, so that a resume and the
    // session's changes are taken in the order logind sent them.
    let logind = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(LOGIN1)?
        .build();
    let logind = bounded("subscribing to logind's signals", async {
        Ok(MessageStream::for_match_rule(logind, conn, None).await?)
    })
    .await?;
    let mut signals = futures_util::stream::select(owners, logind);

    let Some(owner) = name_owner(conn).await? else {
        tracing::info!("no logind on the system bus: counting the user as at the desktop");
        set(true);
        while let Some(msg) = signals.next().await {
            if matches!(signal(&msg?), Some(Signal::Owner { has_owner: true })) {
                return Ok(());
            }
        }
        return Ok(());
    };

    let mut followed = None;
    let mut backoff = Backoff::default();
    let mut warned = false;
    // When to look for the session next: a deadline, not a wait, so that
    // logind's signals in between do not put it off.
    let mut retry_at = tokio::time::Instant::now();
    // When the machine went to sleep, until it resumes.
    let mut sleeping: Option<SystemTime> = None;
    loop {
        // Looked for first, then again on the backoff while there is none.
        let msg = tokio::select! {
            biased;
            () = tokio::time::sleep_until(retry_at), if followed.is_none() => {
                match find_session(conn, &owner).await {
                    Ok(found) => {
                        set(found.session.at_desktop());
                        followed = Some(found);
                    }
                    Err(e) => {
                        if std::mem::replace(&mut warned, true) {
                            tracing::debug!("still no session of the user's to follow: {e:#}");
                        } else {
                            tracing::warn!("no session of the user's to follow, so at the desktop: {e:#}");
                        }
                        set(true);
                        retry_at = tokio::time::Instant::now() + backoff.failed();
                    }
                }
                continue;
            }
            msg = signals.next() => msg,
        };
        let Some(msg) = msg else {
            return Ok(());
        };
        match signal(&msg?) {
            Some(Signal::Owner { .. }) => return Ok(()),
            Some(Signal::Changed {
                sender,
                path: from,
                changed,
                invalidated,
            }) if sender == owner => {
                let Some(Followed { id, path, session }) = followed.as_mut() else {
                    continue;
                };
                if from != path.as_str() {
                    continue;
                }
                session.apply(&changed);
                if invalidated
                    .iter()
                    .any(|name| ["Active", "IdleHint", "LockedHint"].contains(&name.as_str()))
                {
                    session.apply(&get_all(conn, &owner, path.as_str()).await?);
                }
                tracing::debug!("login session {id}: {session:?}");
                set(session.at_desktop());
            }
            Some(Signal::Sleep {
                sender,
                path: from,
                start,
            }) if sender == owner && from == MANAGER_PATH => {
                let now = SystemTime::now();
                if start {
                    tracing::info!("going to sleep");
                    sleeping = Some(now);
                } else {
                    let slept = sleeping.take().unwrap_or(now);
                    tracing::info!("resumed from sleep");
                    presence.send_modify(|p| {
                        p.resumes.count += 1;
                        p.resumes.slept = Some(slept);
                    });
                }
            }
            _ => {}
        }
    }
}

/// The session [`watch`] follows.
struct Followed {
    id: String,
    path: OwnedObjectPath,
    session: Session,
}

/// The session `session/auto` names, at its own path, and its hints as read.
async fn find_session(conn: &Connection, owner: &str) -> Result<Followed> {
    let id: String = get(conn, owner, AUTO_SESSION, "Id")
        .await
        .context("no session of the user's")?;
    let path: OwnedObjectPath = bounded("finding the session", async {
        let manager = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
            .destination(owner.to_string())?
            .path(MANAGER_PATH)?
            .interface(MANAGER_INTERFACE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        manager
            .call_with_flags(
                "GetSession",
                MethodFlags::NoAutoStart.into(),
                &(id.as_str(),),
            )
            .await?
            .context("logind sent no reply")
    })
    .await?;
    let mut session = Session::default();
    session.apply(&get_all(conn, owner, &path).await?);
    tracing::info!(
        "following login session {id}: {}",
        if session.at_desktop() {
            "at the desktop"
        } else {
            "away"
        }
    );
    Ok(Followed { id, path, session })
}

/// A signal [`watch`] takes.
enum Signal {
    /// `org.freedesktop.login1` changed hands.
    Owner { has_owner: bool },
    /// A session's properties, from the unique name `sender`.
    Changed {
        sender: String,
        path: String,
        changed: HashMap<String, OwnedValue>,
        invalidated: Vec<String>,
    },
    /// `PrepareForSleep(start)`: the machine going to sleep, or resumed.
    /// From the unique name `sender`, at `path`.
    Sleep {
        sender: String,
        path: String,
        start: bool,
    },
}

/// The [`Signal`] `msg` is, if it is one: `NameOwnerChanged` only from the
/// bus itself, a session's `PropertiesChanged` and `PrepareForSleep` from
/// whichever sender, for [`watch`] to judge.
fn signal(msg: &Message) -> Option<Signal> {
    let header = msg.header();
    let sender = header.sender()?.as_str();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus", "NameOwnerChanged") => {
            if sender != "org.freedesktop.DBus" {
                return None;
            }
            let (name, _, new) = msg.body().deserialize::<(String, String, String)>().ok()?;
            (name == LOGIN1).then_some(Signal::Owner {
                has_owner: !new.is_empty(),
            })
        }
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path = header.path()?.to_string();
            let (iface, changed, invalidated) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            (iface == SESSION_INTERFACE).then(|| Signal::Changed {
                sender: sender.to_string(),
                path,
                changed,
                invalidated,
            })
        }
        (MANAGER_INTERFACE, "PrepareForSleep") => {
            let start: bool = msg.body().deserialize().ok()?;
            let path = header.path()?.to_string();
            Some(Signal::Sleep {
                sender: sender.to_string(),
                path,
                start,
            })
        }
        _ => None,
    }
}

/// The unique name that owns `org.freedesktop.login1`; `None` while nobody
/// does.
async fn name_owner(conn: &Connection) -> Result<Option<String>> {
    bounded("asking who logind is", async {
        let bus = zbus::fdo::DBusProxy::builder(conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let owner: zbus::Result<Option<String>> = bus
            .inner()
            .call_with_flags("GetNameOwner", MethodFlags::NoAutoStart.into(), &(LOGIN1,))
            .await;
        match owner {
            Ok(owner) => Ok(Some(owner.context("the bus sent no reply")?)),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    })
    .await
}

/// A session property at `path`, from `owner`.
async fn get<T>(conn: &Connection, owner: &str, path: &str, name: &str) -> Result<T>
where
    T: TryFrom<OwnedValue>,
    T::Error: Into<zbus::zvariant::Error>,
{
    bounded("reading the session", async {
        let value: OwnedValue = properties(conn, owner, path)
            .await?
            .inner()
            .call_with_flags(
                "Get",
                MethodFlags::NoAutoStart.into(),
                &(SESSION_INTERFACE, name),
            )
            .await?
            .context("logind sent no reply")?;
        T::try_from(value)
            .map_err(Into::into)
            .with_context(|| format!("the session's {name} is of another type"))
    })
    .await
}

/// Every session property at `path`, from `owner`.
async fn get_all(
    conn: &Connection,
    owner: &str,
    path: &str,
) -> Result<HashMap<String, OwnedValue>> {
    bounded("reading the session", async {
        properties(conn, owner, path)
            .await?
            .inner()
            .call_with_flags(
                "GetAll",
                MethodFlags::NoAutoStart.into(),
                &(SESSION_INTERFACE,),
            )
            .await?
            .context("logind sent no reply")
    })
    .await
}

async fn properties(
    conn: &Connection,
    owner: &str,
    path: &str,
) -> Result<zbus::fdo::PropertiesProxy<'static>> {
    Ok(zbus::fdo::PropertiesProxy::builder(conn)
        .destination(owner.to_string())?
        .path(path.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

/// `call`, unless it takes longer than [`CALL_LIMIT`].
async fn bounded<T>(what: &str, call: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(CALL_LIMIT, call)
        .await
        .with_context(|| format!("no answer within {CALL_LIMIT:?} {what}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PropertiesChanged` that carries one hint changes that one alone.
    #[test]
    fn a_change_keeps_what_it_leaves_out() {
        let mut session = Session::default();
        let changed =
            |name: &str, value: bool| HashMap::from([(name.to_string(), OwnedValue::from(value))]);
        session.apply(&changed("LockedHint", true));
        assert!(!session.at_desktop());
        session.apply(&changed("IdleHint", true));
        session.apply(&changed("LockedHint", false));
        assert!(!session.at_desktop(), "still idle");
        session.apply(&changed("IdleHint", false));
        assert!(session.at_desktop());
        session.apply(&changed("Active", false));
        assert!(!session.at_desktop(), "another session in the foreground");
    }

    /// Against a fake logind on a private bus.
    #[cfg(feature = "harness")]
    mod bus {
        use std::sync::{Arc, Mutex};

        use zbus::interface;
        use zbus::object_server::SignalEmitter;

        use super::*;
        use crate::harness::PrivateBus;

        const TIMEOUT: Duration = Duration::from_secs(5);
        const SESSION_PATH: &str = "/org/freedesktop/login1/session/_32";

        #[derive(Debug, Clone, Copy)]
        struct Hints {
            active: bool,
            idle: bool,
            locked: bool,
        }

        /// The session, at both its own path and `auto`, as logind serves it.
        struct FakeSession(Arc<Mutex<Hints>>);

        #[interface(name = "org.freedesktop.login1.Session")]
        impl FakeSession {
            #[zbus(property)]
            fn id(&self) -> String {
                "2".to_string()
            }
            #[zbus(property)]
            fn active(&self) -> bool {
                self.0.lock().unwrap().active
            }
            #[zbus(property)]
            fn idle_hint(&self) -> bool {
                self.0.lock().unwrap().idle
            }
            #[zbus(property)]
            fn locked_hint(&self) -> bool {
                self.0.lock().unwrap().locked
            }
        }

        struct FakeManager;

        #[interface(name = "org.freedesktop.login1.Manager")]
        impl FakeManager {
            #[zbus(signal)]
            async fn prepare_for_sleep(
                emitter: &SignalEmitter<'_>,
                start: bool,
            ) -> zbus::Result<()>;

            /// A signal of logind's that the watch takes nothing from.
            #[zbus(signal)]
            async fn prepare_for_shutdown(
                emitter: &SignalEmitter<'_>,
                start: bool,
            ) -> zbus::Result<()>;

            fn get_session(&self, id: &str) -> zbus::fdo::Result<OwnedObjectPath> {
                if id != "2" {
                    return Err(zbus::fdo::Error::Failed(format!("no session {id}")));
                }
                Ok(OwnedObjectPath::try_from(SESSION_PATH).unwrap())
            }
        }

        /// A fake logind on `bus`, its session's hints in the `Mutex`.
        async fn logind(bus: &PrivateBus, hints: Hints) -> (Connection, Arc<Mutex<Hints>>) {
            let hints = Arc::new(Mutex::new(hints));
            let conn = zbus::connection::Builder::address(bus.address())
                .unwrap()
                .serve_at(MANAGER_PATH, FakeManager)
                .unwrap()
                .serve_at(SESSION_PATH, FakeSession(hints.clone()))
                .unwrap()
                .serve_at(AUTO_SESSION, FakeSession(hints.clone()))
                .unwrap()
                .name(LOGIN1)
                .unwrap()
                .build()
                .await
                .unwrap();
            (conn, hints)
        }

        /// Changes the session's hints, signaled as logind signals them: at
        /// the session's own path.
        async fn set(conn: &Connection, hints: &Mutex<Hints>, change: impl FnOnce(&mut Hints)) {
            set_at(conn, SESSION_PATH, hints, change).await;
        }

        /// Changes the hints of the session at `path`, signaled there.
        async fn set_at(
            conn: &Connection,
            path: &str,
            hints: &Mutex<Hints>,
            change: impl FnOnce(&mut Hints),
        ) {
            change(&mut hints.lock().unwrap());
            let iface = conn
                .object_server()
                .interface::<_, FakeSession>(path)
                .await
                .unwrap();
            let emitter: &SignalEmitter<'_> = iface.signal_emitter();
            let session = iface.get().await;
            session.active_changed(emitter).await.unwrap();
            session.idle_hint_changed(emitter).await.unwrap();
            session.locked_hint_changed(emitter).await.unwrap();
        }

        /// Away, no resume yet: where a test starts that waits for the
        /// watch to set the user at the desktop.
        fn away() -> Presence {
            Presence {
                at_desktop: false,
                ..Presence::default()
            }
        }

        async fn until(presence: &mut watch::Receiver<Presence>, want: bool) {
            tokio::time::timeout(TIMEOUT, presence.wait_for(|p| p.at_desktop == want))
                .await
                .unwrap_or_else(|_| panic!("never {want}"))
                .unwrap();
        }

        /// The session as read, then each hint as it changes: locked, idle,
        /// or another session in the foreground is away.
        #[tokio::test]
        async fn follows_the_session() {
            let bus = PrivateBus::start().unwrap();
            let (logind, hints) = logind(
                &bus,
                Hints {
                    active: true,
                    idle: false,
                    locked: true,
                },
            )
            .await;
            let (tx, mut presence) = watch::channel(Presence::default());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });

            until(&mut presence, false).await;
            set(&logind, &hints, |h| h.locked = false).await;
            until(&mut presence, true).await;
            set(&logind, &hints, |h| h.idle = true).await;
            until(&mut presence, false).await;
            set(&logind, &hints, |h| h.idle = false).await;
            until(&mut presence, true).await;
            set(&logind, &hints, |h| h.active = false).await;
            until(&mut presence, false).await;
            watching.abort();
        }

        /// Each `PrepareForSleep(false)` from logind counts one resume; a
        /// `PrepareForSleep(true)`, the machine going to sleep, none, but
        /// the time it came is when the sleep began. The unlock after them,
        /// in the same stream, is taken after them.
        #[tokio::test]
        async fn counts_each_resume() {
            let bus = PrivateBus::start().unwrap();
            let (logind, hints) = logind(
                &bus,
                Hints {
                    active: true,
                    idle: false,
                    locked: true,
                },
            )
            .await;
            let (tx, mut presence) = watch::channel(Presence::default());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });
            // Subscribed by the time the session has been read.
            until(&mut presence, false).await;

            let manager = SignalEmitter::new(&logind, MANAGER_PATH).unwrap();
            for start in [true, false] {
                FakeManager::prepare_for_sleep(&manager, start)
                    .await
                    .unwrap();
            }
            let before = SystemTime::now();
            for start in [true, false] {
                FakeManager::prepare_for_sleep(&manager, start)
                    .await
                    .unwrap();
            }
            set(&logind, &hints, |h| h.locked = false).await;
            until(&mut presence, true).await;
            let taken = presence.borrow_and_update().resumes;
            assert_eq!(taken.count, 2);
            let slept = taken.slept.expect("a sleep began");
            assert!(before <= slept && slept <= SystemTime::now(), "{slept:?}");
            watching.abort();
        }

        /// A session's `PropertiesChanged` from another session's path, and
        /// a `PrepareForSleep` from another path than the manager's, are not
        /// taken. Each is followed by a signal that is, and checked once
        /// that one has been taken: the other session locking leaves the
        /// user at the desktop, and the stray resume leaves the count at the
        /// manager's one.
        #[tokio::test]
        async fn takes_signals_from_their_own_paths_alone() {
            const OTHER_SESSION: &str = "/org/freedesktop/login1/session/_33";
            let bus = PrivateBus::start().unwrap();
            let unlocked = Hints {
                active: true,
                idle: false,
                locked: false,
            };
            let (logind, hints) = logind(&bus, unlocked).await;
            let other = Arc::new(Mutex::new(unlocked));
            logind
                .object_server()
                .at(OTHER_SESSION, FakeSession(other.clone()))
                .await
                .unwrap();
            let (tx, mut presence) = watch::channel(away());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });
            until(&mut presence, true).await;

            set_at(&logind, OTHER_SESSION, &other, |h| h.locked = true).await;
            let manager = SignalEmitter::new(&logind, MANAGER_PATH).unwrap();
            FakeManager::prepare_for_sleep(&manager, false)
                .await
                .unwrap();
            tokio::time::timeout(TIMEOUT, presence.wait_for(|p| p.resumes.count > 0))
                .await
                .expect("the manager's resume was never counted")
                .unwrap();
            assert!(presence.borrow().at_desktop, "another session's lock taken");

            let elsewhere =
                SignalEmitter::new(&logind, "/org/freedesktop/login1/elsewhere").unwrap();
            FakeManager::prepare_for_sleep(&elsewhere, false)
                .await
                .unwrap();
            set(&logind, &hints, |h| h.locked = true).await;
            until(&mut presence, false).await;
            assert_eq!(presence.borrow().resumes.count, 1);
            watching.abort();
        }

        /// A session that is not there at first is found once it is,
        /// though logind signals far more often than the backoff's wait: a
        /// signal every 100 ms, the next look a second after the first.
        #[tokio::test]
        async fn finds_a_session_that_comes_later_while_logind_signals() {
            let bus = PrivateBus::start().unwrap();
            let logind = zbus::connection::Builder::address(bus.address())
                .unwrap()
                .serve_at(MANAGER_PATH, FakeManager)
                .unwrap()
                .name(LOGIN1)
                .unwrap()
                .build()
                .await
                .unwrap();
            let (tx, mut presence) = watch::channel(away());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });
            // At the desktop: the first look found no session.
            until(&mut presence, true).await;

            let emitter = SignalEmitter::new(&logind, MANAGER_PATH)
                .unwrap()
                .into_owned();
            let signalling = tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_millis(100));
                loop {
                    every.tick().await;
                    FakeManager::prepare_for_shutdown(&emitter, false)
                        .await
                        .unwrap();
                }
            });
            let hints = Arc::new(Mutex::new(Hints {
                active: true,
                idle: false,
                locked: true,
            }));
            for path in [SESSION_PATH, AUTO_SESSION] {
                logind
                    .object_server()
                    .at(path, FakeSession(hints.clone()))
                    .await
                    .unwrap();
            }
            until(&mut presence, false).await;
            signalling.abort();
            watching.abort();
        }

        /// With no session of the user's - `session/auto` not there - the
        /// user is at the desktop, and each resume is still counted, its
        /// sleep begun when `PrepareForSleep(false)` came, none having come
        /// before it.
        #[tokio::test]
        async fn without_a_session_resumes_are_still_counted() {
            let bus = PrivateBus::start().unwrap();
            let logind = zbus::connection::Builder::address(bus.address())
                .unwrap()
                .serve_at(MANAGER_PATH, FakeManager)
                .unwrap()
                .name(LOGIN1)
                .unwrap()
                .build()
                .await
                .unwrap();
            let (tx, mut presence) = watch::channel(away());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });
            until(&mut presence, true).await;

            let before = SystemTime::now();
            let manager = SignalEmitter::new(&logind, MANAGER_PATH).unwrap();
            FakeManager::prepare_for_sleep(&manager, false)
                .await
                .unwrap();
            let taken = tokio::time::timeout(TIMEOUT, presence.wait_for(|p| p.resumes.count > 0))
                .await
                .expect("the resume was never counted")
                .unwrap()
                .resumes;
            assert_eq!(taken.count, 1);
            let slept = taken.slept.expect("a sleep began");
            assert!(before <= slept && slept <= SystemTime::now(), "{slept:?}");
            assert!(!watching.is_finished(), "the watch gave up");
            watching.abort();
        }

        /// With no logind on a bus that would start it, the user is at the
        /// desktop, nothing asks the bus to start logind, and the watch ends
        /// once logind takes its name, to be set up again.
        #[tokio::test]
        async fn without_logind_at_the_desktop_and_nothing_started() {
            let bus = PrivateBus::activating(LOGIN1).unwrap();
            let (tx, mut presence) = watch::channel(away());
            let conn = bus.connect().await.unwrap();
            let watching = tokio::spawn(async move { watch(&conn, &tx).await });

            until(&mut presence, true).await;
            assert_eq!(bus.activations(), 0);

            let _logind = logind(
                &bus,
                Hints {
                    active: true,
                    idle: false,
                    locked: false,
                },
            )
            .await;
            tokio::time::timeout(TIMEOUT, watching)
                .await
                .expect("the watch ended")
                .unwrap()
                .unwrap();
            assert_eq!(bus.activations(), 0);
        }
    }
}
