//! A tiny XML element tree with deterministic, indented output. Domain XML is
//! write-mostly and small; this avoids coupling the builder to a streaming
//! writer API and makes golden tests trivial.

use std::fmt::Write;

#[derive(Clone, Debug, Default)]
pub struct El {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<El>,
    text: Option<String>,
}

impl El {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn attr(mut self, k: &str, v: impl ToString) -> Self {
        self.attrs.push((k.into(), v.to_string()));
        self
    }

    pub fn text(mut self, t: impl ToString) -> Self {
        self.text = Some(t.to_string());
        self
    }

    pub fn child(mut self, c: El) -> Self {
        self.children.push(c);
        self
    }

    pub fn children(mut self, cs: impl IntoIterator<Item = El>) -> Self {
        self.children.extend(cs);
        self
    }

    pub fn maybe(self, c: Option<El>) -> Self {
        match c {
            Some(c) => self.child(c),
            None => self,
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        let _ = write!(out, "{pad}<{}", self.name);
        for (k, v) in &self.attrs {
            let _ = write!(out, " {k}='{}'", escape(v));
        }
        match (&self.text, self.children.is_empty()) {
            (None, true) => out.push_str("/>\n"),
            (Some(t), true) => {
                let _ = writeln!(out, ">{}</{}>", escape(t), self.name);
            }
            (text, false) => {
                out.push_str(">\n");
                if let Some(t) = text {
                    let _ = writeln!(out, "{pad}  {}", escape(t));
                }
                for c in &self.children {
                    c.write(out, depth + 1);
                }
                let _ = writeln!(out, "{pad}</{}>", self.name);
            }
        }
    }
}

impl El {
    /// Parses a document into a tree (comments, processing instructions
    /// and whitespace between elements dropped). None if it is not XML.
    pub fn parse(xml: &str) -> Option<El> {
        use quick_xml::events::Event;
        let el = |e: &quick_xml::events::BytesStart<'_>| {
            let mut out = El::new(e.name().as_ref());
            for a in e.attributes().flatten() {
                let v = a
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()?
                    .into_owned();
                out.attrs.push((a.key.as_ref().to_owned(), v));
            }
            Some(out)
        };
        // Each open element, with its text so far.
        let mut stack: Vec<(El, String)> = vec![];
        let close = |stack: &mut Vec<(El, String)>, (mut done, text): (El, String)| {
            let text = text.trim();
            if !text.is_empty() {
                done.text = Some(text.to_owned());
            }
            match stack.last_mut() {
                Some((parent, _)) => {
                    parent.children.push(done);
                    None
                }
                None => Some(done),
            }
        };
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event().ok()? {
                Event::Start(e) => stack.push((el(&e)?, String::new())),
                Event::Empty(e) => {
                    if let Some(root) = close(&mut stack, (el(&e)?, String::new())) {
                        return Some(root);
                    }
                }
                Event::Text(t) => {
                    if let Some((_, text)) = stack.last_mut() {
                        text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
                    }
                }
                Event::GeneralRef(r) => {
                    let c = match r.as_ref() {
                        "lt" => '<',
                        "gt" => '>',
                        "amp" => '&',
                        "apos" => '\'',
                        "quot" => '"',
                        _ => r.resolve_char_ref().ok()??,
                    };
                    if let Some((_, text)) = stack.last_mut() {
                        text.push(c);
                    }
                }
                Event::End(_) => {
                    let top = stack.pop()?;
                    if let Some(root) = close(&mut stack, top) {
                        return Some(root);
                    }
                }
                Event::Eof => return None,
                _ => {}
            }
        }
    }

    fn get(&self, attr: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == attr)
            .map(|(_, v)| v.as_str())
    }

    fn child_attr(&self, child: &str, attr: &str) -> Option<&str> {
        self.children
            .iter()
            .find(|c| c.name == child)
            .and_then(|c| c.get(attr))
    }
}

/// What the host gives guests beyond the basics, from its domain
/// capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuestSupport {
    /// A TPM 2.0 of their own (swtpm, as libvirt runs it).
    pub tpm: bool,
    /// UEFI Secure Boot (firmware with it, in flash).
    pub secure_boot: bool,
}

/// What `domain_capabilities` (`virConnectGetDomainCapabilities`) says
/// guests can have.
pub fn guest_support(domain_capabilities: &str) -> GuestSupport {
    let Some(caps) = El::parse(domain_capabilities) else {
        return GuestSupport::default();
    };
    let child = |e: &El, name: &str| e.children.iter().find(|c| c.name == name).cloned();
    let values = |e: &El, enum_name: &str| -> Vec<String> {
        e.children
            .iter()
            .filter(|c| c.name == "enum" && c.get("name") == Some(enum_name))
            .flat_map(|c| c.children.iter().filter_map(|v| v.text.clone()))
            .collect()
    };
    let tpm = child(&caps, "devices")
        .and_then(|d| child(&d, "tpm"))
        .filter(|t| t.get("supported") == Some("yes"))
        .is_some_and(|t| {
            values(&t, "backendModel").iter().any(|v| v == "emulator")
                && values(&t, "backendVersion").iter().any(|v| v == "2.0")
        });
    let secure_boot = child(&caps, "os")
        .and_then(|o| child(&o, "loader"))
        .filter(|l| l.get("supported") == Some("yes"))
        .is_some_and(|l| values(&l, "secure").iter().any(|v| v == "yes"));
    GuestSupport { tpm, secure_boot }
}

/// What a device is, to find it in another definition of the domain.
fn device_key(e: &El) -> Option<String> {
    Some(match e.name.as_str() {
        "disk" => format!("disk:{}", e.child_attr("target", "dev")?),
        "interface" => format!(
            "interface:{}",
            e.child_attr("mac", "address")?.to_lowercase()
        ),
        "channel" => format!("channel:{}", e.child_attr("target", "name")?),
        "serial" | "console" => format!(
            "{}:{}",
            e.name,
            e.child_attr("target", "port").unwrap_or("0")
        ),
        "input" => format!("input:{}:{}", e.get("type")?, e.get("bus").unwrap_or("")),
        "video" | "rng" | "memballoon" => e.name.clone(),
        _ => return None,
    })
}

/// A controller of the source's taken over: buses and the like, nothing
/// that reaches anything on the host. Only what places it is kept.
fn layout_controller(c: &El) -> Option<El> {
    let ok = match c.get("type")? {
        "pci" | "usb" | "sata" | "virtio-serial" => true,
        "scsi" => c.get("model") == Some("virtio-scsi"),
        _ => false,
    };
    ok.then(|| El {
        children: c
            .children
            .iter()
            .filter(|x| matches!(x.name.as_str(), "model" | "target" | "address"))
            .cloned()
            .collect(),
        text: None,
        ..c.clone()
    })
}

/// A domain definition made here (`ours`) laid out as the running domain
/// it replaces (`source`, its definition there, as a migration's source
/// sends it): its controllers, and the address of each device both have.
/// What the guest runs on must be laid out alike for its state to load: a
/// definition made afresh puts devices plugged in or out while the domain
/// ran elsewhere. Nothing else is taken from the source: the devices, and
/// what they reach on the host, are ours. Ours as it is if either is not
/// a domain.
pub fn with_layout_of(ours: &str, source: &str) -> String {
    let (Some(mut dom), Some(src)) = (El::parse(ours), El::parse(source)) else {
        return ours.to_owned();
    };
    let Some(src_devices) = src.children.iter().find(|c| c.name == "devices") else {
        return ours.to_owned();
    };
    let Some(devices) = dom.children.iter_mut().find(|c| c.name == "devices") else {
        return ours.to_owned();
    };
    let addresses: std::collections::HashMap<String, &El> = src_devices
        .children
        .iter()
        .filter_map(|d| {
            let address = d.children.iter().find(|c| c.name == "address")?;
            Some((device_key(d)?, address))
        })
        .collect();
    let mut laid_out: Vec<El> = src_devices
        .children
        .iter()
        .filter(|c| c.name == "controller")
        .filter_map(layout_controller)
        .collect();
    for mut d in std::mem::take(&mut devices.children) {
        if d.name == "controller" {
            continue;
        }
        if let Some(address) = device_key(&d).and_then(|k| addresses.get(&k)) {
            d.children.retain(|c| c.name != "address");
            d.children.push((*address).clone());
        }
        laid_out.push(d);
    }
    devices.children = laid_out;
    dom.render()
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The `dev` of each `<disk>`'s `<target>` in a domain's XML, in order.
pub fn disk_targets(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut in_disk = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "disk" => in_disk += 1,
            Ok(Event::End(e)) if e.local_name().as_ref() == "disk" => {
                in_disk = in_disk.saturating_sub(1)
            }
            Ok(Event::Start(e) | Event::Empty(e))
                if in_disk > 0 && e.local_name().as_ref() == "target" =>
            {
                if let Some(dev) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "dev")
                    .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                {
                    out.push(dev.into_owned());
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// A running domain's disks: each target with its source's index, which
/// names the QEMU block nodes libvirt made for it (`libvirt-<index>-format`,
/// and `libvirt-<index>-storage` below it, or alone for raw disks that need
/// no format layer). Disks without a source (an empty CD-ROM drive) are
/// left out.
pub fn disk_sources(xml: &str) -> Vec<(String, u32)> {
    use quick_xml::events::Event;
    let attr = |e: &quick_xml::events::BytesStart<'_>, name: &str| {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .map(|v| v.into_owned())
    };
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    // Inside a disk: its target and its (top) source's index. The first
    // source is the disk's own; backing stores nest their own sources.
    let mut disk: Option<(Option<String>, Option<String>)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "disk" => disk = Some((None, None)),
            Ok(Event::End(e)) if e.local_name().as_ref() == "disk" => {
                if let Some((Some(target), Some(index))) = disk.take()
                    && let Ok(index) = index.parse()
                {
                    out.push((target, index));
                }
            }
            Ok(Event::Start(e) | Event::Empty(e)) if disk.is_some() => {
                let Some((target, index)) = disk.as_mut() else {
                    continue;
                };
                match e.local_name().as_ref() {
                    "target" if target.is_none() => *target = attr(&e, "dev"),
                    "source" if index.is_none() => *index = attr(&e, "index"),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// A domain's disks: each target with where its data is (a file's path,
/// or a network disk's name, such as an RBD `pool/image`).
pub fn disk_paths(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    let attr = |e: &quick_xml::events::BytesStart<'_>, name: &str| {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .map(|v| v.into_owned())
    };
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut disk: Option<(Option<String>, Option<String>)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "disk" => disk = Some((None, None)),
            Ok(Event::End(e)) if e.local_name().as_ref() == "disk" => {
                if let Some((Some(target), source)) = disk.take() {
                    out.push((target, source.unwrap_or_default()));
                }
            }
            Ok(Event::Start(e) | Event::Empty(e)) if disk.is_some() => {
                let Some((target, source)) = disk.as_mut() else {
                    continue;
                };
                match e.local_name().as_ref() {
                    "target" if target.is_none() => *target = attr(&e, "dev"),
                    // The disk's own source; backing stores nest theirs.
                    "source" if source.is_none() => {
                        *source = attr(&e, "file").or_else(|| attr(&e, "name"));
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// The MAC addresses of a domain's interfaces (lower case).
pub fn interface_macs(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut inside = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "interface" => inside = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == "interface" => inside = false,
            Ok(Event::Start(e) | Event::Empty(e)) if inside && e.local_name().as_ref() == "mac" => {
                if let Some(mac) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "address")
                    .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                {
                    out.push(mac.to_lowercase());
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// The disks (targets) a checkpoint (`<domaincheckpoint>`) keeps a dirty
/// bitmap in.
pub fn checkpoint_disks(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "disk" => {
                let attr = |k: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == k)
                        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                        .map(|v| v.into_owned())
                };
                if attr("checkpoint").as_deref() == Some("bitmap")
                    && let Some(name) = attr("name")
                {
                    out.push(name);
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// A checkpoint's description without its `<parent>`, to tell a libvirt
/// that may not know the parent of it again.
pub fn without_parent(xml: &str) -> String {
    match (xml.find("<parent>"), xml.find("</parent>")) {
        (Some(start), Some(end)) if start < end => {
            let end = end + "</parent>".len();
            // The line it was on goes too.
            let end = xml[end..].find('\n').map_or(end, |n| end + n + 1);
            let start = xml[..start].rfind('\n').map_or(start, |n| n + 1);
            format!("{}{}", &xml[..start], &xml[end..])
        }
        _ => xml.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn guest_support_comes_from_domain_capabilities() {
        let debian = include_str!("../tests/golden/domcaps-debian13.xml");
        assert_eq!(
            super::guest_support(debian),
            super::GuestSupport {
                tpm: true,
                secure_boot: true
            }
        );
        // No swtpm (TPM emulator not offered), firmware without Secure Boot.
        let bare = "<domainCapabilities><os supported='yes'><loader supported='yes'>
            <enum name='secure'><value>no</value></enum></loader></os><devices>
            <tpm supported='yes'><enum name='backendModel'><value>passthrough</value></enum>
            <enum name='backendVersion'><value>2.0</value></enum></tpm></devices></domainCapabilities>";
        assert_eq!(super::guest_support(bare), super::GuestSupport::default());
        assert_eq!(super::guest_support("nope"), super::GuestSupport::default());
    }

    #[test]
    fn checkpoint_descriptions() {
        let xml = "<domaincheckpoint>\n  <name>mst-b</name>\n  <parent>\n    <name>mst-a</name>\n  </parent>\n  \
                   <creationTime>1792324940</creationTime>\n  <disks>\n    \
                   <disk name='vda' checkpoint='bitmap' bitmap='mst-b'/>\n    \
                   <disk name='sda' checkpoint='no'/>\n    \
                   <disk name='sdb' checkpoint='bitmap' bitmap='mst-b'/>\n  </disks>\n</domaincheckpoint>\n";
        assert_eq!(super::checkpoint_disks(xml), ["vda", "sdb"]);
        let alone = super::without_parent(xml);
        assert!(
            !alone.contains("parent") && !alone.contains("mst-a"),
            "{alone}"
        );
        assert!(
            alone.starts_with("<domaincheckpoint>\n  <name>mst-b</name>\n  <creationTime>"),
            "{alone}"
        );
        assert_eq!(super::without_parent(&alone), alone);
    }

    #[test]
    fn disk_paths_and_the_scsi_controller() {
        let xml = r#"<domain><devices>
            <controller type='scsi' index='0' model='virtio-scsi'/>
            <disk type='file' device='disk'><source file='/p/volumes/a.qcow2' index='2'>
              <backingStore type='file'><source file='/p/images/i.qcow2'/></backingStore></source>
              <target dev='vda' bus='virtio'/></disk>
            <disk type='network' device='disk'><source protocol='rbd' name='pool/vol-b'/>
              <target dev='sdb' bus='scsi'/></disk>
        </devices></domain>"#;
        assert_eq!(
            super::disk_paths(xml),
            [
                ("vda".to_owned(), "/p/volumes/a.qcow2".to_owned()),
                ("sdb".to_owned(), "pool/vol-b".to_owned())
            ]
        );
    }

    #[test]
    fn disk_sources_index_the_top_source() {
        let xml = r#"<domain><devices>
            <disk type='file' device='disk'><driver name='qemu' type='qcow2'/>
              <source file='/v/a.qcow2' index='3'/>
              <backingStore type='file' index='4'><format type='qcow2'/><source file='/i/b.qcow2'/></backingStore>
              <target dev='vda' bus='virtio'/><alias name='virtio-disk0'/></disk>
            <disk type='network' device='disk'><source protocol='rbd' name='p/v' index='2'><host name='m'/></source>
              <target dev='vdb' bus='virtio'/></disk>
            <disk type='file' device='cdrom'><source file='/s.iso' index='1'/><target dev='sda' bus='sata'/><readonly/></disk>
            <disk type='file' device='cdrom'><target dev='sdb' bus='sata'/></disk>
          </devices></domain>"#;
        assert_eq!(
            super::disk_sources(xml),
            [
                ("vda".to_owned(), 3),
                ("vdb".to_owned(), 2),
                ("sda".to_owned(), 1)
            ]
        );
    }

    #[test]
    fn disk_targets_skip_other_targets() {
        let xml = r#"<domain><devices>
            <disk type='file' device='disk'><source file='/a'/><target dev='vda' bus='virtio'/></disk>
            <disk type='network' device='disk'><source protocol='rbd' name='p/v'/><target dev="vdb" bus="virtio"/></disk>
            <serial type='pty'><target port='0'/></serial>
            <disk type='file' device='cdrom'><target dev='sda' bus='sata'/><readonly/></disk>
            <channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>
          </devices></domain>"#;
        assert_eq!(super::disk_targets(xml), ["vda", "vdb", "sda"]);
    }

    use super::*;

    #[test]
    fn renders_and_escapes() {
        let x = El::new("a")
            .attr("q", "it's <x>")
            .child(El::new("b").text("1 & 2"))
            .child(El::new("c"))
            .render();
        assert_eq!(
            x,
            "<a q='it&apos;s &lt;x&gt;'>\n  <b>1 &amp; 2</b>\n  <c/>\n</a>\n"
        );
    }
}
