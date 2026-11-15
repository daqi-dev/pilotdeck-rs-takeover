use crate::{WAIT, wire::Wire};
use agentcpm_host::{
    EmbedOptions, EmbedTransport, ExternalBackends, SchedulerAssembly, SchedulerComponent,
    SchedulerImplementation, StartupOptions, serve_resident,
};
use agentcpm_iface::{BoxFuture, model_session::*, types::Error};
use agentcpm_runtime::model::managed_http::{ManagedCredential, ManagedHttpTransport};
use agentcpm_scheduler_protocol as p;
use pilotdeck_model_iface::PilotDeckModelFactory;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct ObservedFactory {
    inner: PilotDeckModelFactory,
    sessions: Mutex<Vec<Arc<dyn ModelSession>>>,
}
impl ModelSessionFactory for ObservedFactory {
    fn capabilities(&self) -> SessionCapabilities {
        self.inner.capabilities()
    }
    fn supported_profiles(&self) -> Vec<String> {
        self.inner.supported_profiles()
    }
    fn open(
        &self,
        config: ModelSessionConfig,
    ) -> BoxFuture<'_, Result<Arc<dyn ModelSession>, Error>> {
        Box::pin(async move {
            let session = self.inner.open(config).await?;
            self.sessions.lock().unwrap().push(session.clone());
            Ok(session)
        })
    }
}

pub struct Resident {
    pub root: PathBuf,
    pub addr: SocketAddr,
    factory: Arc<ObservedFactory>,
    thread: Option<std::thread::JoinHandle<i32>>,
}
impl Resident {
    pub fn start(http: SocketAddr) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pilotdeck-resident-model-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("data")).unwrap();
        // 与正式 install@1 schema 一致的 iface-only 声明，不伪造可执行制品摘要。
        let components = root.join("components");
        std::fs::create_dir_all(components.join("fixture-iface")).unwrap();
        let ledger = components.join("install.json");
        std::fs::write(
            &ledger,
            json!({"schema":"agentcpm.kernel/install@1","packages":[{
            "name":"fixture-iface","version":"0.0.0","kind":"iface","dir":"fixture-iface",
            "provides":{"interfaces":["env"]},"sha256":{}}]})
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::write(root.join("kernel.toml"), concat!(
            "storage_dir = \"storage\"\nguard = \"deny-all\"\nexec_backend = \"none\"\n",
            "allow_ops = \"session.new,session.lease_acquire,session.get,session.deliver,session.stream_open,execution.open,execution.input,execution.get,execution.events,model.session_open,model.session_get,model.input_append,model.input_commit,model.session_close,model.session_abort\"\n"
        )).unwrap();
        let program = root.join("pilotdeck-native-model");
        std::fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
        #[cfg(target_os = "macos")]
        {
            assert!(
                std::process::Command::new("codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(&program)
                    .status()
                    .expect("codesign source-built fixture")
                    .success()
            );
            assert!(
                std::process::Command::new(&program)
                    .arg("--version")
                    .status()
                    .expect("warm signed provider")
                    .success()
            );
        }
        let sha256 = format!("{:x}", Sha256::digest(std::fs::read(&program).unwrap()));
        let assembly = SchedulerAssembly {
            protocol: p::PROTOCOL.into(),
            profile: p::SessionConfig {
                input_controller_ref: Some("controller".into()),
                default_loop_ref: "pilotdeck".into(),
                allowed_bindings: ["pilotdeck".into(), "controller".into()].into(),
            },
            bindings: vec![
                SchedulerComponent {
                    binding: p::Binding {
                        id: "pilotdeck".into(),
                        revision: "model-fixture-v1".into(),
                        kind: p::Kind::Loop,
                        effect: p::Effect::Model,
                        permissions: p::Permissions {
                            invoke: ["controller".into()].into(),
                            publish: false,
                            state_namespace: None,
                            resources: [
                                "model.session_open",
                                "model.session_get",
                                "model.input_append",
                                "model.input_commit",
                                "model.events_read",
                                "model.session_close",
                                "model.session_abort",
                            ]
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        },
                    },
                    provider: SchedulerImplementation::Native {
                        program: program.clone(),
                        args: vec!["--native-provider".into()],
                        sha256: sha256.clone(),
                    },
                    controller: false,
                },
                SchedulerComponent {
                    binding: p::Binding {
                        id: "controller".into(),
                        revision: "fixture-v1".into(),
                        kind: p::Kind::Service,
                        effect: p::Effect::State,
                        permissions: p::Permissions {
                            invoke: ["pilotdeck".into()].into(),
                            publish: true,
                            state_namespace: None,
                            resources: Default::default(),
                        },
                    },
                    provider: SchedulerImplementation::Native {
                        program,
                        args: vec!["--native-controller".into()],
                        sha256: sha256.clone(),
                    },
                    controller: true,
                },
            ],
        };
        let origin = format!("http://{http}");
        // cc7c4e6 parseModelConfig.ts:312 / protocol/multimodal.ts:24：
        // 工厂接收已规范化配置；自定义模型默认只允许 text 输入。
        let config = pilotdeck_protocol::parse_json(&json!({"providers":{"p":{"id":"p","protocol":"openai","url":origin,"models":{"m":{"id":"m","capabilities":{"supportsStreaming":true},"multimodal":{"input":["text"]}}}}}}).to_string()).unwrap();
        let factory = Arc::new(ObservedFactory {
            inner: PilotDeckModelFactory {
                config,
                transport: Arc::new(
                    ManagedHttpTransport::new(&origin, ManagedCredential::None, 1024 * 1024)
                        .unwrap(),
                ),
            },
            sessions: Mutex::new(Vec::new()),
        });
        let mut options = EmbedOptions::new(
            StartupOptions {
                config: Some(root.join("kernel.toml")),
                data_dir: root.join("data"),
                components_dir: components,
            },
            EmbedTransport::Listen(([127, 0, 0, 1], 0).into()),
        );
        options.transport_attested_principal = Some("kernel".into());
        options.assume_single_principal = true;
        options.require_attested_tenant = false;
        let (send, receive) = mpsc::channel();
        options.on_ready = Some(Box::new(move |notice| {
            send.send(notice.listen.unwrap()).unwrap();
        }));
        let installed = factory.clone();
        let thread = std::thread::spawn(move || {
            serve_resident(
                options,
                ExternalBackends {
                    scheduler_assembly: Some(assembly),
                    model_session_factory: Some(installed),
                    ..Default::default()
                },
            )
        });
        let addr = receive.recv_timeout(WAIT).expect("public resident ready");
        let host = Self {
            root,
            addr,
            factory,
            thread: Some(thread),
        };
        host.save("assembly.json", &json!({"native_sha256":sha256,"native_args":["--native-provider"],"http_origin":origin,"host_default_features":false}));
        host
    }
    pub fn save(&self, name: &str, value: &Value) {
        std::fs::write(
            self.root.join(name),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
    pub fn assert_model_closed(&self) {
        let sessions = self.factory.sessions.lock().unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "one real factory open; no replacement default backend"
        );
        assert_eq!(
            sessions[0].state(),
            SessionState::Closed,
            "model close must be confirmed independently of Loop success"
        );
        self.save(
            "model-sessions.json",
            &json!([{"model_session_id":sessions[0].model_session_id().0,"state":"closed"}]),
        );
    }
    pub fn shutdown(mut self) -> PathBuf {
        self.stop().expect("resident clean shutdown");
        self.root.clone()
    }
    fn stop(&mut self) -> Result<(), String> {
        if self.thread.is_none() {
            return Ok(());
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Wire::new(self.addr).call("shutdown", json!({}));
        }));
        let end = Instant::now() + WAIT;
        while self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let code = self
                .thread
                .take()
                .unwrap()
                .join()
                .map_err(|_| "resident panicked")?;
            if code != 0 {
                return Err(format!("resident exit {code}"));
            }
            Ok(())
        } else {
            Err(format!(
                "resident stop unconfirmed; evidence retained {}",
                self.root.display()
            ))
        }
    }
}
impl Drop for Resident {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("{error}");
        }
        // 临时目录含实测请求/结果/产物摘要，交由验收收据收集后清理。
    }
}
