//! Wire-format checks for generated types.

use mst_virtrpc::protocol::{self, NodeGetInfoRet, TypedParam, TypedParamValue};
use mst_virtrpc::xdr;

#[test]
fn char_arrays_use_four_bytes_per_element() {
    let info = NodeGetInfoRet {
        model: vec![b'x' as i8; 32],
        memory: 1 << 20,
        cpus: 4,
        ..Default::default()
    };
    let bytes = xdr::to_bytes(&info).unwrap();
    // 32 chars x 4 bytes, one hyper, six ints.
    assert_eq!(bytes.len(), 32 * 4 + 8 + 6 * 4);
    assert_eq!(&bytes[..4], &[0, 0, 0, b'x']);
    assert_eq!(xdr::from_bytes::<NodeGetInfoRet>(&bytes).unwrap(), info);

    let short = NodeGetInfoRet {
        model: vec![0; 3],
        ..Default::default()
    };
    assert!(
        xdr::to_bytes(&short).is_err(),
        "fixed arrays must have their exact length"
    );
}

#[test]
fn typed_parameters_roundtrip() {
    for value in [
        TypedParamValue::VirTypedParamInt(-3),
        TypedParamValue::VirTypedParamUllong(u64::MAX),
        TypedParamValue::VirTypedParamDouble(0.5),
        TypedParamValue::VirTypedParamString("x".into()),
    ] {
        let p = TypedParam {
            field: "migrate_disks".into(),
            value,
        };
        assert_eq!(
            xdr::from_bytes::<TypedParam>(&xdr::to_bytes(&p).unwrap()).unwrap(),
            p
        );
    }
    // Discriminant first, then the arm.
    let bytes = xdr::to_bytes(&TypedParamValue::VirTypedParamUint(7)).unwrap();
    assert_eq!(bytes, [0, 0, 0, 2, 0, 0, 0, 7]);
}

#[test]
fn procedure_numbers_match_libvirt() {
    assert_eq!(protocol::proc::CONNECT_OPEN, 1);
    assert_eq!(protocol::proc::AUTH_POLKIT, 70);
    assert_eq!(protocol::proc::DOMAIN_OPEN_CONSOLE, 201);
    assert_eq!(protocol::proc::DOMAIN_GET_STATE, 212);
    assert_eq!(
        protocol::proc::CONNECT_DOMAIN_EVENT_CALLBACK_REGISTER_ANY,
        316
    );
    assert_eq!(protocol::proc::DOMAIN_EVENT_CALLBACK_LIFECYCLE, 318);
    assert_eq!(protocol::proc::DOMAIN_OPEN_GRAPHICS_FD, 343);
}
