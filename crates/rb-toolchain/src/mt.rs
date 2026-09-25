//! A tiny `mt.exe` for lld-link's `/MANIFEST:EMBED` when the host has no libxml2.
//!
//! Keep the last manifest's `<assembly>` start tag (its children use those namespace prefixes),
//! then every child of the user manifests, then children from earlier manifests whose name
//! nobody later defined. That's usually lld's default `<trustInfo>`.

use anyhow::{Context, Result, bail};

struct Manifest {
    root_start: String,
    children: Vec<(String, String)>,
}

fn skip_misc(s: &str, mut i: usize) -> usize {
    loop {
        let rest = &s[i..];
        let trimmed = rest.trim_start();
        i += rest.len() - trimmed.len();
        if trimmed.starts_with("<?") {
            i += trimmed.find("?>").map(|p| p + 2).unwrap_or(trimmed.len());
        } else if trimmed.starts_with("<!--") {
            i += trimmed.find("-->").map(|p| p + 3).unwrap_or(trimmed.len());
        } else {
            return i;
        }
    }
}

fn tag_end(s: &str, i: usize) -> Option<usize> {
    let mut quote = None;
    for (off, c) in s[i..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i + off + 1),
            _ => {}
        }
    }
    None
}

fn local_name(tag: &str) -> String {
    let name: String = tag
        .trim_start_matches(['<', '/'])
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
        .collect();
    name.rsplit(':').next().unwrap_or(&name).to_owned()
}

fn parse(xml: &str) -> Result<Manifest> {
    let start = skip_misc(xml, 0);
    let root_end = tag_end(xml, start).context("manifest has no root element")?;
    let root_start = xml[start..root_end].to_owned();
    let mut children = Vec::new();
    let mut i = root_end;
    loop {
        i = skip_misc(xml, i);
        if i >= xml.len() || xml[i..].starts_with("</") {
            break;
        }
        if !xml[i..].starts_with('<') {
            i += xml[i..].find('<').unwrap_or(xml.len() - i);
            continue;
        }
        let begin = i;
        let first_end = tag_end(xml, i).context("unterminated tag")?;
        let name = local_name(&xml[i..first_end]);
        let mut depth = if xml[..first_end].ends_with("/>") { 0 } else { 1 };
        i = first_end;
        while depth > 0 {
            let next = xml[i..].find('<').map(|p| i + p).context("unterminated element")?;
            if xml[next..].starts_with("<!--") {
                i = next + xml[next..].find("-->").map(|p| p + 3).context("unterminated comment")?;
                continue;
            }
            let end = tag_end(xml, next).context("unterminated tag")?;
            if xml[next..].starts_with("</") {
                depth -= 1;
            } else if !xml[..end].ends_with("/>") && !xml[next..].starts_with("<?") {
                depth += 1;
            }
            i = end;
        }
        children.push((name, xml[begin..i].to_owned()));
    }
    Ok(Manifest { root_start, children })
}

pub fn merge(manifests: &[String]) -> Result<String> {
    let parsed: Vec<Manifest> = manifests.iter().map(|m| parse(m)).collect::<Result<_>>()?;
    let Some(last) = parsed.last() else {
        bail!("no manifests to merge")
    };
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n{}\n",
        last.root_start
    );
    for (i, m) in parsed.iter().enumerate() {
        for (name, text) in &m.children {
            let overridden = parsed[i + 1..].iter().any(|later| later.children.iter().any(|(n, _)| n == name));
            if !overridden {
                out.push_str("  ");
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out.push_str("</assembly>\n");
    Ok(out)
}

/// Entry point for `rb __mt <mt.exe arguments>`
pub fn run(args: &[String]) -> Result<i32> {
    let mut inputs = Vec::new();
    let mut out = None;
    let mut in_manifests = false;
    // A Unix path also starts with `/`. Only known flags count as flags.
    for a in args {
        let lower = a.to_ascii_lowercase();
        let flag = lower.strip_prefix('/').or_else(|| lower.strip_prefix('-')).unwrap_or("");
        if flag == "manifest" {
            in_manifests = true;
        } else if flag.starts_with("out:") {
            out = Some(a[5..].to_owned());
            in_manifests = false;
        } else if flag == "nologo" {
            in_manifests = false;
        } else if in_manifests {
            inputs.push(a.clone());
        }
    }
    let out = out.context("mt: missing /out:<file>")?;
    let texts: Vec<String> = inputs
        .iter()
        .map(|p| std::fs::read_to_string(p).with_context(|| format!("mt: cannot read {p}")))
        .collect::<Result<_>>()?;
    std::fs::write(&out, merge(&texts)?).with_context(|| format!("mt: cannot write {out}"))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "<?xml version=\"1.0\" standalone=\"yes\"?>\n<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\"\n          manifestVersion=\"1.0\">\n  <trustInfo>\n    <security>\n      <requestedPrivileges>\n         <requestedExecutionLevel level='asInvoker' uiAccess='false'/>\n      </requestedPrivileges>\n    </security>\n  </trustInfo>\n</assembly>\n";

    #[test]
    fn keeps_default_trust_info_and_all_user_children() {
        let user = "<?xml version=\"1.0\"?>\n<!-- c -->\n<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\" xmlns:asmv3=\"urn:schemas-microsoft-com:asm.v3\">\n  <compatibility><application><supportedOS Id=\"{x}\"/></application></compatibility>\n  <!-- a -->\n  <asmv3:application><asmv3:windowsSettings><a>1</a></asmv3:windowsSettings></asmv3:application>\n  <asmv3:application><asmv3:windowsSettings><b>2</b></asmv3:windowsSettings></asmv3:application>\n</assembly>\n";
        let merged = merge(&[DEFAULT.to_owned(), user.to_owned()]).unwrap();
        assert!(merged.contains("xmlns:asmv3"));
        assert!(merged.contains("requestedExecutionLevel"));
        assert_eq!(merged.matches("<asmv3:application>").count(), 2);
        assert!(merged.contains("<compatibility>"));
    }

    #[test]
    fn later_manifest_overrides_same_element() {
        let user = "<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\"><trustInfo><security>admin</security></trustInfo></assembly>";
        let merged = merge(&[DEFAULT.to_owned(), user.to_owned()]).unwrap();
        assert!(merged.contains("admin") && !merged.contains("asInvoker"));
    }

    #[test]
    fn parses_lld_invocation() {
        let dir = std::env::temp_dir().join(format!("rb-mt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, out) = (dir.join("a.manifest"), dir.join("b.manifest"), dir.join("out.manifest"));
        std::fs::write(&a, DEFAULT).unwrap();
        std::fs::write(
            &b,
            "<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\"><compatibility/></assembly>",
        )
        .unwrap();
        let args: Vec<String> = [
            "/manifest",
            a.to_str().unwrap(),
            "/manifest",
            b.to_str().unwrap(),
            "/nologo",
            &format!("/out:{}", out.display()),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run(&args).unwrap(), 0);
        let merged = std::fs::read_to_string(&out).unwrap();
        assert!(merged.contains("<compatibility/>") && merged.contains("trustInfo"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
