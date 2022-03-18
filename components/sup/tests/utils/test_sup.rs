//! Encapsulate running the `hab-sup` executable for tests.

use crate::hcore::url::BLDR_URL_ENVVAR;
use anyhow::{anyhow,
             Context,
             Result};
use hyper::Method;
use rand::{self,
           distributions::{Distribution,
                           Uniform}};
use serde_json::Value;
use std::{collections::HashSet,
          env,
          io,
          net::{Ipv4Addr,
                SocketAddrV4},
          path::{Path,
                 PathBuf},
          process::Stdio,
          string::ToString,
          time::Duration};
use tokio::{net::{TcpListener,
                  TcpStream},
            process::{Child,
                      Command},
            sync::Mutex,
            time::Instant};

use super::test_butterfly;

lazy_static! {
    /// Keep track of all TCP ports currently being used by TestSup
    /// instances. Allows us to run tests in parallel without fear of
    /// port conflicts between them.
    static ref CLAIMED_PORTS: Mutex<HashSet<u16>> = {
        Mutex::new(HashSet::new())
    };
}

pub struct TestSup {
    pub hab_root:         PathBuf,
    pub http_port:        u16,
    pub butterfly_port:   u16,
    pub control_port:     u16,
    pub butterfly_client: test_butterfly::Client,
    pub api_client:       reqwest::Client,
    pub cmd:              Command,
    pub process:          Option<Child>,
}

/// Return a free TCP port number. We test to see that the system has
/// not already bound the port, while also tracking which ports are
/// being used by other test supervisors that may be running alongside
/// this one.
///
/// Once you receive a port number from this function, you can be
/// reasonably sure that you're the only one that will be using
/// it. There could be a race condition if the machine the tests are
/// running on just happens to claim the same port number for
/// something between the time we check and the time the TestSup
/// claims it. If that happens to you, you should probably buy lottery
/// tickets, though.
///
/// This function will recursively call itself with a decremented
/// value for `tries` if it happens to pick a port that's already in
/// use. Once all tries are used up, it panics! Yay!
async fn unclaimed_port(max_attempts: u16) -> Result<u16> {
    let mut attempts = 0;
    loop {
        let port = random_port();
        match TcpListener::bind(format!("127.0.0.1:{}", port)).await {
            Ok(_listener) => {
                // The system hasn't bound it. Now we make sure none of
                // our other tests have bound it.
                let mut claimed_ports = CLAIMED_PORTS.lock().await;
                if claimed_ports.contains(&port) {
                    // Oops, another test is using it, try again
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    attempts += 1;
                } else {
                    // Nobody was using it. Return the port; the TcpListener
                    // that is currently bound to the port will be dropped,
                    // thus freeing the port for our use.
                    claimed_ports.insert(port);
                    return Ok(port);
                }
            }
            // If the port is in use carry on
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                attempts += 1;
            }
            // If we are unable to bind for any other reason, bubble that up
            Err(err) => {
                return Err(anyhow!(err)).with_context(|| {
                                            format!("Failed to bind TCP port {} due to io error",
                                                    port)
                                        });
            }
        }
        if attempts > max_attempts {
            return Err(anyhow!("Failed to find an unclaimed TCP port in {} \
                                attempts",
                               max_attempts));
        }
    }
}

/// Return a random unprivileged, unregistered TCP port number.
fn random_port() -> u16 {
    // IANA port registrations go to 49151
    let between = Uniform::new_inclusive(49152, ::std::u16::MAX);
    let mut rng = rand::thread_rng();
    between.sample(&mut rng)
}

/// Find an executable relative to the current integration testing
/// executable.
///
/// Thus if the current executable is
///
///    /home/me/habitat/target/debug/deps/compilation-ccaf2f45c24e3840
///
/// and we look for `hab-sup`, we'll find it at
///
///    /home/me/habitat/target/debug/hab-sup
fn find_exe<B>(binary_name: B) -> Result<PathBuf>
    where B: AsRef<Path>
{
    let exe_root = env::current_exe().context("Failed to find the integration test executable")?
        .parent() // deps
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("Failed to access the parent directories of the current integration test executable"))?
        .to_path_buf();
    let bin = exe_root.join(binary_name.as_ref());
    if bin.exists() {
        Ok(bin)
    } else {
        Err(anyhow!("Failed to find executable '{}'", bin.display()))
    }
}

/// Return whether or not the tests are being run with the `--nocapture` flag meaning we want to
/// see more output.
fn nocapture_set() -> bool {
    if env::args().any(|arg| arg == "--nocapture") {
        true
    } else {
        match env::var("RUST_TEST_NOCAPTURE") {
            Ok(val) => &val != "0",
            Err(_) => false,
        }
    }
}

async fn await_local_tcp_port(port: u16, timeout: Duration) -> Result<()> {
    let started_at = Instant::now();
    loop {
        let timeout = timeout.saturating_sub(started_at.elapsed());
        if timeout == Duration::ZERO {
            return Err(anyhow!("Timed out waiting for tcp port {} to open up", port));
        }
        match tokio::time::timeout(timeout,
                                   TcpStream::connect(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0,
                                                                                      1),
                                                                        port))).await
        {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(err)) if err.kind() == io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Ok(Err(err)) => {
                return Err(anyhow!(err)).with_context(|| {
                                            format!("Failed to connect to tcp address 127.0.0.1:{}",
                                                    port)
                                        })
            }
            Err(_) => return Err(anyhow!("Timed out waiting for tcp port {} to open up", port)),
        }
    }
}

impl TestSup {
    /// Create a new `TestSup` that will listen on randomly-selected
    /// ports for both gossip and HTTP requests so tests run in
    /// parallel don't step on each other.
    ///
    /// See also `new`.
    pub async fn new_with_random_ports<R>(fs_root: R,
                                          service_min_backoff_period: Duration,
                                          service_max_backoff_period: Duration,
                                          service_restart_cooldown_period: Duration)
                                          -> Result<TestSup>
        where R: AsRef<Path>
    {
        // We'll give 10 tries to find a free port number
        let http_port =
            unclaimed_port(10).await
                              .context("Failed to allocate an unclaimed port for the \
                                        supervisor HTTP server")?;
        let butterfly_port =
            unclaimed_port(10).await
                              .context("Failed to allocate an unclaimed port for the \
                                        supervisor Butterfly server")?;
        let control_port =
            unclaimed_port(10).await
                              .context("Failed to allocate an unclaimed port for the \
                                        supervisor Control Gateway server")?;

        TestSup::new(fs_root,
                     http_port,
                     butterfly_port,
                     control_port,
                     service_min_backoff_period,
                     service_max_backoff_period,
                     service_restart_cooldown_period)
    }

    /// Bundle up a Habitat Supervisor process along with an
    /// associated Butterfly client for injecting new configuration
    /// values. The Supervisor executable is the one that has been
    /// compiled for the current `cargo test` invocation.
    ///
    /// The Supervisor is configured to run a single package for a
    /// test. This package is assumed to have already been installed
    /// relative to `fs_root` (i.e., the `FS_ROOT` environment
    /// variable, which in our tests will be a randomly-named
    /// temporary directory that this Supervisor will view as `/`.).
    ///
    /// A Butterfly client is also created for interacting with this
    /// Supervisor and package. It is properly configured according to
    /// the value provided for `butterfly_port`. To use it, see the
    /// `apply_config` function.
    ///
    /// (No HTTP interaction with the Supervisor is currently called
    /// for, so we don't have a HTTP client.)
    pub fn new<R>(fs_root: R,
                  http_port: u16,
                  butterfly_port: u16,
                  control_port: u16,
                  service_min_backoff_period: Duration,
                  service_max_backoff_period: Duration,
                  service_restart_cooldown_period: Duration)
                  -> Result<TestSup>
        where R: AsRef<Path>
    {
        let sup_exe = find_exe("hab-sup").context("Failed to find 'hab-sup' executable")?;
        let launcher_exe =
            find_exe("hab-launch").context("Failed to find 'hab-launch' executable")?;

        let mut cmd = Command::new(&launcher_exe);
        let listen_host = "0.0.0.0";

        cmd.env(
            "FS_ROOT",
            fs_root.as_ref().to_string_lossy().as_ref(),
        )
        // .env("HAB_INTERPRETER_IDENT", format!("{}/{}", origin, pkg_name))
        .env("HAB_SUP_BINARY", &sup_exe)
        .env(BLDR_URL_ENVVAR, "https://bldr.habitat.sh")
        .env("HAB_BLDR_CHANNEL", "dev")
        .arg("run")
        .arg("--listen-gossip")
        .arg(format!("{}:{}", listen_host, butterfly_port))
        .arg("--listen-http")
        .arg(format!("{}:{}", listen_host, http_port))
        .arg("--listen-ctl")
        .arg(format!("{}:{}", listen_host, control_port))
        .arg("--service-min-backoff-period")
        .arg(format!("{}", service_min_backoff_period.as_secs()))
        .arg("--service-max-backoff-period")
        .arg(format!("{}", service_max_backoff_period.as_secs()))
        .arg("--service-restart-cooldown-period")
        .arg(format!("{}", service_restart_cooldown_period.as_secs()))
        // Note: we will have already dropped off the spec files
        // needed to run our test service, so we don't supply a
        // package identifier here
        .stdin(Stdio::null());
        if !nocapture_set() {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
        cmd.kill_on_drop(true);

        let bc = test_butterfly::Client::new(butterfly_port);
        let api_client =
            reqwest::ClientBuilder::new().build()
                                         .context("Failed to create reqwest API client for \
                                                   test supervisor")?;
        Ok(TestSup { hab_root: fs_root.as_ref().to_path_buf(),
                     http_port,
                     butterfly_port,
                     control_port,
                     butterfly_client: bc,
                     api_client,
                     cmd,
                     process: None })
    }

    /// Spawn a process actually running the Supervisor.
    pub async fn start(&mut self, timeout: Duration) -> Result<()> {
        let started_at = Instant::now();
        let child = self.cmd
                        .spawn()
                        .context("Failed to spawn supervisor process")?;
        self.process = Some(child);
        let timeout = timeout.saturating_sub(started_at.elapsed());
        tokio::try_join!(await_local_tcp_port(self.http_port, timeout),
                         await_local_tcp_port(self.butterfly_port, timeout),
                         await_local_tcp_port(self.control_port, timeout)
                        ).context("Timed out waiting for test supervisor to start")?;
        Ok(())
    }

    /// Stop the Supervisor.
    pub async fn stop(mut self) -> Result<()> {
        let mut claimed_ports = CLAIMED_PORTS.lock().await;
        claimed_ports.remove(&self.http_port);
        claimed_ports.remove(&self.butterfly_port);
        claimed_ports.remove(&self.control_port);
        if let Some(mut process) = self.process.take() {
            process.kill()
                   .await
                   .context("Failed to kill supervisor process")?;
        }
        Ok(())
    }

    /// The equivalent of performing `hab apply` with the given
    /// configuration.
    pub async fn apply_config(&mut self, package_name: &str, service_group: &str, toml_config: &str) {
        self.butterfly_client.apply(package_name, service_group, toml_config)
    }

    pub async fn wait_for_service_startup(&self,
                                          package_name: &str,
                                          service_group: &str,
                                          timeout: Duration)
                                          -> Result<u64> {
        let started_at = Instant::now();
        loop {
            if started_at.elapsed() > timeout {
                return Err(anyhow!("Test supervisor failed to start service '{}.{}' \
                                    within {:.2}secs",
                                   package_name,
                                   service_group,
                                   timeout.as_secs_f64()));
            } 

            let req = self.api_client
                          .request(Method::GET,
                                   format!("http://localhost:{}/services/{}/{}",
                                           self.http_port, package_name, service_group).as_str())
                          .build()
                          .context("Failed to construct API request to supervisor HTTP endpoint")?;
            let res = self.api_client.execute(req).await.ok();

            let body = if let Some(res) = res {
                res.json::<Value>().await.ok()
            } else {
                continue;
            };
            let body = if let Some(body) = body {
                body
            } else {
                continue;
            };
            if let (Some("up"), Some(process_id)) = (body.get("process")
                                                         .and_then(|x| x.get("state"))
                                                         .and_then(|x| x.as_str()),
                                                     body.get("process")
                                                         .and_then(|x| x.get("pid"))
                                                         .and_then(|x| x.as_u64()))
            {
                return Ok(process_id);
            } 
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn wait_for_service_restart(&self,
                                          old_process_id: u64,
                                          package_name: &str,
                                          service_group: &str,
                                          timeout: Duration)
                                          -> Result<u64> {
        let started_at = Instant::now();
        loop {
            if started_at.elapsed() > timeout {
                return Err(anyhow!("Test supervisor failed to restart service '{}.{}' \
                                    within {:.2}secs",
                                   package_name,
                                   service_group,
                                   timeout.as_secs_f64()));
            } 
            let req = self.api_client
                          .request(Method::GET,
                                   format!("http://localhost:{}/services/{}/{}",
                                           self.http_port, package_name, service_group).as_str())
                          .build()
                          .context("Failed to construct API request to supervisor HTTP endpoint")?;
            let res = self.api_client.execute(req).await.ok();

            let body = if let Some(res) = res {
                res.json::<Value>().await.ok()
            } else {
                continue;
            };
            let body = if let Some(body) = body {
                body
            } else {
                continue;
            };

            if let (Some("up"), Some(process_id)) = (body.get("process")
                                                         .and_then(|x| x.get("state"))
                                                         .and_then(|x| x.as_str()),
                                                     body.get("process")
                                                         .and_then(|x| x.get("pid"))
                                                         .and_then(|x| x.as_u64()))
            {
                if process_id != old_process_id {
                    return Ok(process_id);
                }
            }
            
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
