//! P2 only: actual Resident Host and original Gateway ToolRuntime, driven by a
//! controlled test Loop. This is not the product NativeCodec/P5 integration.
#![cfg(all(unix, feature = "kernel-managed-model"))]
use crate::{
    extensions::NativeExtensions,
    native_process::{
        DeploymentKernelConfig, GatewayManagedModelSetup, KernelSessionBinder, LocalGateway,
        LocalOptions, NativeGatewayServices, NativeToolBatchSetup, OriginalProjectCatalogPolicy,
        OriginalProjectTurnAuthority,
    },
    runtime::turn::kernel_client::KernelExecutionOptions,
};
use agentcpm_host::{
    EmbedOptions, ExternalBackends, ResidentComposition, SchedulerAssembly, SchedulerComponent,
    SchedulerImplementation, StartupOptions, serve_resident_composed,
};
use agentcpm_scheduler_protocol as p;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
struct ResidentGuard {
    stop: agentcpm_host::ResidentShutdown,
    thread: Option<std::thread::JoinHandle<i32>>,
}
impl Drop for ResidentGuard {
    fn drop(&mut self) {
        self.stop.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn stop_fences_original_batch_before_second_ordinal() {
    // Process-level isolation also covers Host libraries that consult HOME;
    // mutating the test runner's process-global environment is unsafe.
    if std::env::var_os("PILOTDECK_P2_STOP_CHILD").is_none() {
        let isolated = std::env::temp_dir().join(format!("pd-p2-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&isolated).unwrap();
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let receipt = isolated.join("finished");
        let log_path = isolated.join("fixture.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_process::p2_stop_fixture_tests::stop_fences_original_batch_before_second_ordinal",
                "--nocapture",
            ])
            .env("PILOTDECK_P2_STOP_CHILD", "1")
            .env("PILOTDECK_P2_STOP_RECEIPT", &receipt)
            .env("HOME", &isolated)
            .env("PILOT_HOME", &isolated)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()
            .expect("start isolated P2 fixture");
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait().expect("poll isolated fixture") {
                Some(status) => break status,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                None => {
                    if let Some(group) = Pid::from_raw(child.id() as i32) {
                        let _ = kill_process_group(group, Signal::KILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "isolated fixture timed out; retained log: {}",
                        log_path.display()
                    );
                }
            }
        };
        if !status.success() {
            if let Some(group) = Pid::from_raw(child.id() as i32) {
                let _ = kill_process_group(group, Signal::KILL);
            }
        }
        assert!(
            status.success(),
            "isolated fixture failed: {status}; retained log: {}",
            log_path.display()
        );
        assert_eq!(
            std::fs::read(&receipt).expect("test child did not publish completion receipt"),
            b"completed"
        );
        std::fs::remove_dir_all(isolated).unwrap();
        return;
    }
    let executable = PathBuf::from(
        std::env::var_os("PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE")
            .expect("source-built managed_native helper required"),
    );
    assert!(executable.is_absolute() && executable.is_file());
    let root = std::env::temp_dir().join(format!("pd-p2-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let project = root.join("project");
    let components = root.join("components");
    for path in [
        &home,
        &project,
        &components.join("fixture-iface"),
        &root.join("data"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let file = project.join("fixture.txt");
    std::fs::write(&file, "PILOTDECK_REAL_TOOL_FIXTURE_MARKER\n").unwrap();
    std::fs::write(components.join("install.json"), json!({"schema":"agentcpm.kernel/install@1","packages":[{"name":"fixture-iface","version":"0.0.0","kind":"iface","dir":"fixture-iface","provides":{"interfaces":["env"]},"sha256":{}}]}).to_string()).unwrap();
    let config = root.join("kernel.toml");
    std::fs::write(&config, "storage_dir = \"storage\"\nguard = \"deny-all\"\nexec_backend = \"none\"\nallow_ops = \"session.create,session.close,session.lease_acquire,session.lease_renew,session.get,session.deliver,session.stream_open,execution.open,execution.input,execution.get,execution.events,execution.output_ack,execution.control,tool.exec\"\n").unwrap();
    let digest = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
    let component =
        |id: &str, kind: p::Kind, resources: &[&str], args: Vec<String>, controller: bool| {
            SchedulerComponent {
                binding: p::Binding {
                    id: id.into(),
                    revision: "fixture-v1".into(),
                    kind,
                    effect: if controller {
                        p::Effect::State
                    } else {
                        p::Effect::ToolWrite
                    },
                    permissions: p::Permissions {
                        invoke: if controller {
                            ["pilotdeck".into()].into()
                        } else {
                            ["controller".into()].into()
                        },
                        publish: controller,
                        state_namespace: None,
                        resources: resources.iter().map(|v| (*v).into()).collect(),
                    },
                },
                provider: SchedulerImplementation::Native {
                    program: executable.clone(),
                    args,
                    sha256: digest.clone(),
                },
                controller,
            }
        };
    let assembly = SchedulerAssembly {
        protocol: p::PROTOCOL.into(),
        profile: p::SessionConfig {
            input_controller_ref: Some("controller".into()),
            default_loop_ref: "pilotdeck".into(),
            allowed_bindings: ["pilotdeck".into(), "controller".into()].into(),
        },
        bindings: vec![
            component(
                "pilotdeck",
                p::Kind::Loop,
                &[
                    p::tool_batch::PREPARE,
                    p::tool_batch::STATUS,
                    p::tool_batch::CLOSE,
                ],
                vec![
                    "--p2-tool-fixture".into(),
                    file.to_str().unwrap().into(),
                    root.join("stop-exchange").to_str().unwrap().into(),
                    "stop".into(),
                ],
                false,
            ),
            component(
                "controller",
                p::Kind::Service,
                &[],
                vec!["--p2-product-controller".into()],
                true,
            ),
        ],
    };
    let executor = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let setup = Arc::new(GatewayManagedModelSetup::new(
        None,
        Arc::new(OriginalProjectTurnAuthority),
        executor.handle().clone(),
    ));
    let composition = ResidentComposition::new(ExternalBackends {
        scheduler_assembly: Some(assembly),
        ..Default::default()
    })
    .with_managed_model(setup.clone())
    .with_trusted_tool_batches(Arc::new(NativeToolBatchSetup));
    let (composition, shutdown) = composition.with_shutdown_control();
    let uid: u32 = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();
    let deployment = DeploymentKernelConfig {
        socket: root.join("host.sock"),
        expected_uid: uid,
        principal: "gateway".into(),
        tenant: "fixture".into(),
        rpc_timeout_ms: 10000,
    };
    let mut options: EmbedOptions = deployment
        .host_options(StartupOptions {
            config: Some(config),
            data_dir: root.join("data"),
            components_dir: components,
        })
        .unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    options.on_ready = Some(Box::new(move |_| {
        ready_tx.send(()).unwrap();
    }));
    let thread = std::thread::spawn(move || serve_resident_composed(options, composition));
    let mut resident = ResidentGuard {
        stop: shutdown,
        thread: Some(thread),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("resident readiness");
    let binder = Arc::new(KernelSessionBinder {
        connections: Arc::new(deployment),
        project_sessions: setup.project_sessions().unwrap(),
        catalog: Arc::new(OriginalProjectCatalogPolicy),
        turn_options: Arc::new(|_, _| {
            Ok(KernelExecutionOptions {
                loop_binding: "pilotdeck".into(),
                controller_binding: "controller".into(),
                module_id: "fixture".into(),
                cleanup_timeout: Duration::from_secs(15),
            })
        }),
    });
    let pilot_config = home.join("pilotdeck.yaml");
    std::fs::write(&pilot_config, "schemaVersion: 1\nagent: {model: fixture/mock}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:9/v1\n      apiKey: fixture-local-only\n      models: {mock: {capabilities: {supportsToolUse: true}}}\ntelemetry: {enabled: false}\n").unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            pilot_config.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    let gateway = LocalGateway::start_with_services(
        LocalOptions {
            pilot_home: home.clone(),
            project,
            os_home: Some(home),
            sidecar: root.join("unused"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        },
        NativeExtensions::default(),
        NativeGatewayServices {
            kernel_execution: Some(binder),
            ..Default::default()
        },
    )
    .unwrap();
    // This guard can only observe this exact fresh project file. It does not
    // issue a permit or supply a handler result; it holds the real first
    // continuation before the Host candidate until we observe CLOSE/held.
    let (entered_tx, entered_rx) = mpsc::channel();
    let barrier = super::managed_tool_backend::test_barrier::install(
        file.to_str().unwrap().to_owned(),
        entered_tx,
    );
    struct BarrierGuard(Arc<super::managed_tool_backend::test_barrier::Barrier>);
    impl Drop for BarrierGuard {
        fn drop(&mut self) {
            super::managed_tool_backend::test_barrier::uninstall(&self.0);
        }
    }
    let guard = BarrierGuard(barrier.clone());
    let request = pilotdeck_protocol::parse_json(r#"{"sessionKey":"p2-stop-session","channelKey":"test","message":"controlled stopped tool batch"}"#).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    let turn = std::thread::spawn(move || {
        let result = gateway.submit_turn(
            request,
            Arc::new(move |event| capture.lock().unwrap().push(event)),
        );
        (gateway, result)
    });
    entered_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("original first continuation reached barrier");
    assert_eq!(barrier.count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let gate = root.join("stop-exchange");
    std::fs::write(gate.with_extension("go"), b"go").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !gate.with_extension("closed").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "original scoped client did not confirm CLOSE with resources held"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(barrier.count.load(std::sync::atomic::Ordering::SeqCst), 1);
    barrier.release.notify_one();
    let (gateway, result) = turn.join().expect("gateway turn thread");
    result.expect("stopped batch reached real release");
    assert_eq!(
        barrier.count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "stop admitted another original ordinal"
    );
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.get("type") == Some(&pilotdeck_protocol::JsValue::from("error"))),
        "stopped turn emitted error"
    );
    assert_eq!(
        *barrier.candidate_rejected.lock().unwrap(),
        Some(true),
        "closed original Host candidate did not reject"
    );
    assert_eq!(
        barrier
            .action_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "original action continuation was invoked after close"
    );
    drop(guard);
    gateway
        .shutdown(Duration::from_secs(15))
        .expect("gateway cleanup");
    drop(gateway);
    resident.stop.request_shutdown();
    assert_eq!(
        resident
            .thread
            .take()
            .unwrap()
            .join()
            .expect("resident thread"),
        0
    );
    drop(executor);
    std::fs::remove_dir_all(root).unwrap();
    let receipt = std::env::var_os("PILOTDECK_P2_STOP_RECEIPT").expect("parent receipt path");
    std::fs::write(receipt, b"completed").unwrap();
}
