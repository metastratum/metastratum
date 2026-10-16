//! Lifecycle against libvirt's in-process test driver (no daemon or KVM
//! needed). Each `test:///default` connection gets a fresh mock host.

use hm_libvirt::{DomainDef, Firmware, Libvirt, Power};

fn def(uuid: &str) -> DomainDef {
    DomainDef {
        uuid: uuid.into(),
        tenant_id: "0199f0a2-7b3c-7d00-8000-0000000000aa".into(),
        title: "lifecycle".into(),
        generation: 1,
        vcpus: 1,
        memory_mib: 256,
        // The test driver has no firmware descriptors to autoselect from.
        firmware: Firmware::Bios,
        disks: vec![],
        nics: vec![],
    }
}

#[tokio::test]
async fn define_start_stop_undefine() {
    let lv = Libvirt::new("test:///default");
    let uuid = "0199f0a2-7b3c-7d00-8000-000000000042";

    lv.define(
        def(uuid)
            .to_xml()
            .replace("<domain type='kvm'>", "<domain type='test'>"),
    )
    .await
    .unwrap();

    let managed = lv.list_managed().await.unwrap();
    let ours: Vec<_> = managed.iter().filter(|d| d.meta.vm_id == uuid).collect();
    assert_eq!(ours.len(), 1, "metadata must be readable back: {managed:?}");
    assert_eq!(ours[0].meta.generation, 1);
    assert_eq!(ours[0].power, Power::Stopped);
    // The driver's built-in "test" domain carries no hypermon metadata.
    assert!(managed.iter().all(|d| d.meta.vm_id == uuid));

    lv.start(uuid).await.unwrap();
    lv.start(uuid).await.unwrap(); // idempotent
    assert_eq!(lv.power(uuid).await.unwrap(), Power::Running);

    lv.destroy(uuid).await.unwrap();
    assert_eq!(lv.power(uuid).await.unwrap(), Power::Stopped);

    lv.undefine(uuid).await.unwrap();
    lv.undefine(uuid).await.unwrap(); // idempotent
    assert!(matches!(
        lv.power(uuid).await,
        Err(hm_libvirt::conn::Error::NoDomain(_))
    ));
}
