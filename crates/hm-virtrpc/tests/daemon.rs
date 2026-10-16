//! Against a real libvirt daemon. Runs when `HM_TEST_LIBVIRT_SOCKET` points
//! at one, e.g. `/run/libvirt/virtqemud-sock`; the user needs access to
//! `qemu:///system` (root, or the `libvirt` group via polkit). Creates and
//! removes a small throwaway domain with no disks.

use std::io::Read;
use std::time::Duration;

use hm_virtrpc::protocol::*;
use hm_virtrpc::{Client, err, xdr};

const VIR_DOMAIN_EVENT_DEFINED: i32 = 0;
const VIR_DOMAIN_EVENT_UNDEFINED: i32 = 1;
const VIR_DOMAIN_EVENT_STARTED: i32 = 2;
const VIR_DOMAIN_EVENT_STOPPED: i32 = 5;
const VIR_DOMAIN_RUNNING: i32 = 1;

/// Waits for lifecycle event `want` on domain `name`, skipping others (a
/// start, for instance, also emits RESUMED).
async fn expect_lifecycle(
    events: &mut tokio::sync::broadcast::Receiver<hm_virtrpc::Event>,
    name: &str,
    want: i32,
) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .unwrap_or_else(|_| panic!("no lifecycle event {want} for {name}"))
            .unwrap();
        if ev.proc != proc::DOMAIN_EVENT_CALLBACK_LIFECYCLE {
            continue;
        }
        let msg: DomainEventCallbackLifecycleMsg = xdr::from_bytes(&ev.body).unwrap();
        if msg.msg.dom.name == name && msg.msg.event == want {
            return;
        }
    }
}

#[tokio::test]
async fn domain_lifecycle_events_graphics_and_console() {
    let Ok(socket) = std::env::var("HM_TEST_LIBVIRT_SOCKET") else {
        return eprintln!("HM_TEST_LIBVIRT_SOCKET not set; skipping");
    };
    let c = Client::connect(&socket, "qemu:///system")
        .await
        .expect("connect");

    let host = c.connect_get_hostname().await.unwrap();
    let info = c.node_get_info().await.unwrap();
    assert!(!host.hostname.is_empty());
    assert!(info.cpus > 0 && info.memory > 0, "{info:?}");

    let reg = c
        .connect_domain_event_callback_register_any(&ConnectDomainEventCallbackRegisterAnyArgs {
            event_id: 0,
            dom: None,
        })
        .await
        .unwrap();
    let mut events = c.events();

    let name = format!("hm-virtrpc-test-{}", std::process::id());
    let xml = format!(
        "<domain type='kvm'><name>{name}</name><memory unit='MiB'>64</memory><vcpu>1</vcpu>\
         <os><type arch='x86_64' machine='q35'>hvm</type></os>\
         <devices><serial type='pty'/><console type='pty'/>\
         <graphics type='vnc'><listen type='none'/></graphics><video><model type='vga'/></video></devices></domain>"
    );
    let dom = c
        .domain_define_xml(&DomainDefineXmlArgs { xml })
        .await
        .unwrap()
        .dom;
    expect_lifecycle(&mut events, &name, VIR_DOMAIN_EVENT_DEFINED).await;

    let result = tokio::spawn({
        let (c, dom, name) = (c.clone(), dom.clone(), name.clone());
        let mut events = c.events();
        async move {
            c.domain_create(&DomainCreateArgs { dom: dom.clone() })
                .await
                .unwrap();
            expect_lifecycle(&mut events, &name, VIR_DOMAIN_EVENT_STARTED).await;
            let st = c
                .domain_get_state(&DomainGetStateArgs {
                    dom: dom.clone(),
                    flags: 0,
                })
                .await
                .unwrap();
            assert_eq!(st.state, VIR_DOMAIN_RUNNING);

            // The VNC server arrives as a passed file descriptor.
            let ((), fds) = c
                .call_with_fds::<_, ()>(
                    proc::DOMAIN_OPEN_GRAPHICS_FD,
                    &DomainOpenGraphicsFdArgs {
                        dom: dom.clone(),
                        idx: 0,
                        flags: 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(fds.len(), 1);
            let mut vnc = std::os::unix::net::UnixStream::from(fds.into_iter().next().unwrap());
            vnc.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut banner = [0u8; 12];
            vnc.read_exact(&mut banner).unwrap();
            assert!(banner.starts_with(b"RFB 003."), "{banner:?}");

            // The serial console is a stream; it opens and can be written to.
            let stream = c
                .open_stream(
                    proc::DOMAIN_OPEN_CONSOLE,
                    &DomainOpenConsoleArgs {
                        dom: dom.clone(),
                        dev_name: None,
                        flags: 1,
                    },
                )
                .await
                .unwrap();
            stream.tx.send(b"\r").await.unwrap();
            drop(stream);

            c.domain_destroy(&DomainDestroyArgs { dom: dom.clone() })
                .await
                .unwrap();
            expect_lifecycle(&mut events, &name, VIR_DOMAIN_EVENT_STOPPED).await;
        }
    })
    .await;

    // Clean up even if the scenario failed.
    let _ = c
        .domain_destroy(&DomainDestroyArgs { dom: dom.clone() })
        .await;
    c.domain_undefine(&DomainUndefineArgs { dom: dom.clone() })
        .await
        .unwrap();
    expect_lifecycle(&mut events, &name, VIR_DOMAIN_EVENT_UNDEFINED).await;
    let missing = c
        .domain_lookup_by_uuid(&DomainLookupByUuidArgs { uuid: dom.uuid })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Some(err::NO_DOMAIN), "{missing}");

    c.connect_domain_event_callback_deregister_any(&ConnectDomainEventCallbackDeregisterAnyArgs {
        callback_id: reg.callback_id,
    })
    .await
    .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic.into_panic());
    }
}
