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

#[cfg(test)]
mod tests {
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
