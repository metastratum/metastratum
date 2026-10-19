//! Against a real libvirt daemon, when `MST_TEST_LIBVIRT_SOCKET` is set (e.g.
//! `/run/libvirt/virtqemud-sock`). Defines, starts and removes a small
//! throwaway domain with no disks or NICs.

use std::time::Duration;

use mst_libvirt::{Change, DomainDef, Firmware, Libvirt, Power};
use tokio::sync::mpsc::Receiver;

fn def(uuid: &str) -> DomainDef {
    DomainDef {
        uuid: uuid.into(),
        tenant_id: "0199f0a2-7b3c-7d00-8000-0000000000aa".into(),
        title: "mst-libvirt lifecycle test".into(),
        generation: 1,
        vcpus: 1,
        max_vcpus: 1,
        vcpu_hotplug: false,
        memory_mib: 64,
        max_memory_mib: 64,
        free_page_reporting: false,
        firmware: Firmware::Bios,
        disks: vec![],
        nics: vec![],
        cpu: None,
        host_cache: false,
        disk_limits: Default::default(),
    }
}

async fn expect_change(rx: &mut Receiver<Change>, uuid: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let c = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("no change event")
            .unwrap();
        if c == Change::Domain(uuid.into()) {
            return;
        }
    }
}

#[tokio::test]
async fn define_start_stop_undefine_with_events() {
    let Ok(socket) = std::env::var("MST_TEST_LIBVIRT_SOCKET") else {
        return eprintln!("MST_TEST_LIBVIRT_SOCKET not set; skipping");
    };
    let lv = Libvirt::new(socket, "qemu:///system");
    let uuid = uuid::Uuid::now_v7().to_string();
    let mut changes = lv.watch();
    assert_eq!(changes.recv().await, Some(Change::Resync));

    let facts = lv.node_facts().await.unwrap();
    assert!(facts.cpus > 0 && facts.memory_mib > 0, "{facts:?}");

    lv.define(def(&uuid).to_xml()).await.unwrap();
    expect_change(&mut changes, &uuid).await;
    // Run the scenario in a task so a failed assertion still cleans up.
    let result = tokio::spawn({
        let (lv, uuid) = (lv.clone(), uuid.clone());
        async move {
            let managed = lv.list_managed().await.unwrap();
            let ours: Vec<_> = managed.iter().filter(|d| d.meta.vm_id == uuid).collect();
            assert_eq!(ours.len(), 1, "metadata must be readable back");
            assert_eq!(
                (ours[0].meta.generation, ours[0].state.power),
                (1, Power::Stopped)
            );

            lv.start(&uuid).await.unwrap();
            lv.start(&uuid).await.unwrap(); // idempotent
            assert_eq!(lv.power(&uuid).await.unwrap(), Power::Running);

            let vnc = std::os::unix::net::UnixStream::from(lv.open_vnc(&uuid).await.unwrap());
            vnc.set_nonblocking(true).unwrap();
            let mut vnc = tokio::net::UnixStream::from_std(vnc).unwrap();
            let mut banner = [0u8; 12];
            tokio::io::AsyncReadExt::read_exact(&mut vnc, &mut banner)
                .await
                .unwrap();
            assert!(banner.starts_with(b"RFB 003."));

            let serial = lv.open_serial(&uuid).await.unwrap();
            serial.tx.send(b"\r").await.unwrap();

            lv.destroy(&uuid).await.unwrap();
            let state = lv.state(&uuid).await.unwrap();
            assert_eq!(
                (state.power, state.reason),
                (Power::Stopped, Some("destroyed"))
            );
        }
    })
    .await;
    lv.undefine(&uuid).await.unwrap();
    lv.undefine(&uuid).await.unwrap(); // idempotent
    assert!(matches!(
        lv.power(&uuid).await,
        Err(mst_libvirt::conn::Error::NoDomain(_))
    ));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic.into_panic());
    }
    // Start, stop and undefine each produced events too.
    expect_change(&mut changes, &uuid).await;
}
