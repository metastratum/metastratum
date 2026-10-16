//! Guest CPU models that every host of a cluster can run, so VMs can migrate
//! between them (what VMware calls EVC).
//!
//! Each host reports its *host-model* CPU (libvirt's closest named model plus
//! extra features). A cluster's baseline is the model and features all of
//! its hosts share, computed by libvirt; guests get exactly that CPU.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::xml::escape;

/// A named CPU model with feature adjustments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuModel {
    pub model: String,
    pub vendor: Option<String>,
    /// `(policy, name)`: `require` adds a feature, `disable` removes one.
    pub features: Vec<(String, String)>,
}

impl CpuModel {
    /// `<cpu>` for a guest, or for `virConnectBaselineHypervisorCPU` input.
    pub fn to_xml(&self) -> String {
        let mut out = String::from("<cpu mode='custom' match='exact' check='partial'>");
        out += &format!("<model fallback='forbid'>{}</model>", escape(&self.model));
        if let Some(v) = &self.vendor {
            out += &format!("<vendor>{}</vendor>", escape(v));
        }
        for (policy, name) in &self.features {
            out += &format!(
                "<feature policy='{}' name='{}'/>",
                escape(policy),
                escape(name)
            );
        }
        out + "</cpu>"
    }
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Reads model, vendor and features from the first `<cpu>` (or, in domain
/// capabilities, the `<mode name='host-model'>`) element.
fn parse(xml: &str, in_host_model: bool) -> Option<CpuModel> {
    let mut r = Reader::from_str(xml);
    let mut cpu = CpuModel::default();
    // Depth inside the element we read; 0 = not inside yet.
    let mut inside = 0usize;
    let mut text_of: Option<&'static str> = None;
    loop {
        match r.read_event().ok()? {
            Event::Start(e) => {
                let name = e.name();
                if inside == 0 {
                    let wanted = if in_host_model {
                        name.as_ref() == "mode" && attr(&e, "name").as_deref() == Some("host-model")
                    } else {
                        name.as_ref() == "cpu"
                    };
                    if wanted {
                        inside = 1;
                    }
                    continue;
                }
                inside += 1;
                text_of = match name.as_ref() {
                    "model" if inside == 2 => Some("model"),
                    "vendor" if inside == 2 => Some("vendor"),
                    _ => None,
                };
            }
            Event::Empty(e) if inside == 1 && e.name().as_ref() == "feature" => {
                if let (Some(p), Some(n)) = (attr(&e, "policy"), attr(&e, "name")) {
                    cpu.features.push((p, n));
                }
            }
            Event::Text(t) => {
                if let Some(which) = text_of {
                    let v = t.xml_content(XmlVersion::Implicit1_0).trim().to_owned();
                    match which {
                        "model" => cpu.model = v,
                        _ => cpu.vendor = Some(v),
                    }
                }
            }
            Event::End(_) if inside > 0 => {
                text_of = None;
                inside -= 1;
                if inside == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    (!cpu.model.is_empty()).then_some(cpu)
}

/// The host-model CPU from `virConnectGetDomainCapabilities` XML.
pub fn host_model(domain_capabilities: &str) -> Option<CpuModel> {
    parse(domain_capabilities, true)
}

/// A `<cpu>` element, e.g. the result of a baseline computation.
pub fn parse_cpu(xml: &str) -> Option<CpuModel> {
    parse(xml, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: &str = r#"<domainCapabilities>
  <cpu>
    <mode name='host-passthrough' supported='yes'/>
    <mode name='host-model' supported='yes'>
      <model fallback='forbid'>EPYC-Milan</model>
      <vendor>AMD</vendor>
      <maxphysaddr mode='passthrough' limit='48'/>
      <feature policy='require' name='x2apic'/>
      <feature policy='disable' name='xsaves'/>
    </mode>
    <mode name='custom' supported='yes'>
      <model usable='yes'>qemu64</model>
    </mode>
  </cpu>
</domainCapabilities>"#;

    #[test]
    fn host_models_come_from_domain_capabilities() {
        let cpu = host_model(CAPS).unwrap();
        assert_eq!(cpu.model, "EPYC-Milan");
        assert_eq!(cpu.vendor.as_deref(), Some("AMD"));
        assert_eq!(
            cpu.features,
            [
                ("require".into(), "x2apic".into()),
                ("disable".into(), "xsaves".into())
            ]
        );
    }

    #[test]
    fn models_round_trip_through_xml() {
        let cpu = host_model(CAPS).unwrap();
        assert_eq!(parse_cpu(&cpu.to_xml()).unwrap(), cpu);
        assert!(cpu.to_xml().starts_with("<cpu mode='custom' match='exact'"));
    }
}
