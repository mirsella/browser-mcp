use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::RwLock,
};

static KNOWN: RwLock<Vec<String>> = RwLock::new(Vec::new());
const MASK: &str = "[REDACTED]";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Secret {
    value: String,
    origins: Vec<String>,
}

#[derive(Default)]
pub struct Secrets(HashMap<String, Secret>);

impl Secrets {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .context("cannot open configured secrets file")?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o600
            || meta.len() > 64 * 1024
        {
            bail!("secrets file must be a regular, same-user 0600 file, at most 64 KiB");
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            bail!("secrets file grew past 64 KiB");
        }
        // Do not include serde errors: they can contain the rejected value.
        let values: HashMap<String, Secret> = serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("invalid secrets file; expected name: {{value, origins}}")
        })?;
        if values.is_empty() || values.len() > 64 {
            bail!("configure 1..64 secrets");
        }
        let mut redactions = Vec::new();
        for (name, secret) in &values {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            {
                bail!("invalid secret name");
            }
            if secret.value.is_empty()
                || secret.value.len() > 4096
                || secret.value.contains(['\r', '\n'])
                || secret.origins.is_empty()
            {
                bail!(
                    "secret values must be nonempty single lines up to 4096 bytes, with allowed origins"
                );
            }
            for origin in &secret.origins {
                let valid = Url::parse(origin).ok().is_some_and(|url| {
                    url.scheme() == "https" && url.origin().ascii_serialization() == *origin
                });
                if !valid {
                    bail!(
                        "secret origins must be exact HTTPS origins without paths or credentials"
                    );
                }
            }
            redactions.push(secret.value.clone());
            redactions.push(
                serde_json::to_string(&secret.value)?
                    .trim_matches('"')
                    .to_owned(),
            );
            let mut url = Url::parse("https://redaction.invalid").expect("constant URL");
            url.query_pairs_mut().append_pair("v", &secret.value);
            redactions.push(url.query().unwrap().trim_start_matches("v=").to_owned());
        }
        let mut known = KNOWN.write().expect("known-secret redaction lock poisoned");
        known.extend(redactions);
        known.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        known.dedup();
        Ok(Self(values))
    }

    pub fn function(&self, name: &str) -> Result<String> {
        let secret = self
            .0
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown configured secret name"))?;
        let value = serde_json::to_string(&secret.value)?;
        // Page code can replace array methods; origin checks use literal comparisons.
        let mut origins = String::new();
        for origin in &secret.origins {
            if !origins.is_empty() {
                origins.push_str(" || ");
            }
            origins.push_str("location.origin === ");
            origins.push_str(&serde_json::to_string(origin)?);
        }
        Ok(format!(
            r#"(el) => {{
if (window.top !== window) return 'secret-error: top-level frame required';
if (!({origins})) return 'secret-error: origin denied';
if (!el || el.ownerDocument !== document || el.disabled || el.readOnly) return 'secret-error: element not editable';
if (el instanceof HTMLInputElement && !['text','password','email','search','tel','url','number'].includes(el.type)) return 'secret-error: unsupported input type';
if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {{
 const proto = el instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
 Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, {value});
}} else if (el.isContentEditable) el.textContent = {value};
else return 'secret-error: element not editable';
el.dispatchEvent(new Event('input', {{bubbles:true}}));
el.dispatchEvent(new Event('change', {{bubbles:true}}));
return 'ok';
}}"#
        ))
    }
}

pub fn known(text: &str) -> Cow<'_, str> {
    let values = KNOWN.read().expect("known-secret redaction lock poisoned");
    let mut text = Cow::Borrowed(text);
    for value in values.iter() {
        if text.contains(value) {
            text = Cow::Owned(text.replace(value, MASK));
        }
    }
    text
}

fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "cookie",
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "csrf",
        "xsrf",
        "credential",
        "otp",
    ]
    .iter()
    .any(|part| key.contains(part))
}

pub fn url(text: &str) -> String {
    let Ok(mut url) = Url::parse(text) else {
        return known(text).into_owned();
    };
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("REDACTED");
        let _ = url.set_password(None);
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            (
                key.to_string(),
                if sensitive(&key) {
                    MASK.to_owned()
                } else {
                    known(&value).into_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    if !pairs.is_empty() {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    if url
        .fragment()
        .is_some_and(|fragment| fragment.split(['=', '&']).any(sensitive))
    {
        url.set_fragment(Some("REDACTED"));
    }
    known(url.as_str()).into_owned()
}

pub fn network(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if sensitive(key) {
                    *child = Value::String(MASK.into());
                } else if key.eq_ignore_ascii_case("url") {
                    if let Some(text) = child.as_str() {
                        *child = Value::String(url(text));
                    }
                } else {
                    network(child);
                }
            }
        }
        Value::Array(array) => {
            for child in array {
                network(child);
            }
        }
        Value::String(text) => {
            if let Ok(mut body) = serde_json::from_str::<Value>(text)
                && (body.is_object() || body.is_array())
            {
                network(&mut body);
                *text = body.to_string();
                return;
            }
            // Form bodies. Preserve non-form opaque strings, with known-value redaction.
            if text.contains('=')
                && !text.contains(['\n', '{', '<'])
                && text
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .any(|(key, _)| sensitive(key))
            {
                let source = format!("https://redaction.invalid/?{text}");
                if let Ok(parsed) = Url::parse(&url(&source)) {
                    *text = parsed.query().unwrap_or_default().to_owned();
                    return;
                }
            }
            if let Cow::Owned(redacted) = known(text) {
                *text = redacted;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_require_private_regular_files_and_exact_https_origins() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".scratch/secret-validation-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("secrets.json");
        let save = |body: &str, mode| {
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let valid = r#"{"fixture":{"value":"fixture-secret-unique-marker","origins":["https://example.test"]}}"#;
        save(valid, 0o644);
        assert!(Secrets::load(Some(&path)).is_err());
        save(valid, 0o600);
        let secrets = Secrets::load(Some(&path)).unwrap();
        assert!(secrets.function("fixture").is_ok());
        assert!(secrets.function("missing").is_err());
        assert!(!known("fixture-secret-unique-marker").contains("fixture-secret-unique-marker"));
        assert!(matches!(known("ordinary fixture output"), Cow::Borrowed(_)));
        save(&valid.replace("unique-marker", "unique-markes"), 0o600);
        Secrets::load(Some(&path)).unwrap();
        save(valid, 0o600);
        Secrets::load(Some(&path)).unwrap();
        assert_eq!(
            KNOWN
                .read()
                .unwrap()
                .iter()
                .filter(|value| *value == "fixture-secret-unique-marker")
                .count(),
            1
        );
        let link = root.join("linked.json");
        symlink(&path, &link).unwrap();
        assert!(Secrets::load(Some(&link)).is_err());
        for origin in [
            "http://example.test",
            "https://example.test/path",
            "https://user@example.test",
            "https://example.test/",
        ] {
            save(&valid.replace("https://example.test", origin), 0o600);
            assert!(Secrets::load(Some(&path)).is_err());
        }
        save(&"x".repeat(65537), 0o600);
        assert!(Secrets::load(Some(&path)).is_err());
        save("{bad JSON fixture-secret-unique-marker}", 0o600);
        assert!(
            !format!("{:#}", Secrets::load(Some(&path)).err().unwrap())
                .contains("fixture-secret-unique-marker")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn redacts_named_fields_without_destroying_useful_network_data() {
        let mut value = serde_json::json!({"url":"https://example.test/a?q=hello&access_token=hidden", "requestHeaders":{"Authorization":"hidden","Accept":"text/html"}, "requestBody":"{\"password\":\"hidden\",\"name\":\"Ada\"}","responseBody":"otp=hidden&name=Ada"});
        network(&mut value);
        let output = value.to_string();
        assert!(!output.contains("hidden"));
        assert!(output.contains("Ada"));
        assert!(output.contains("text/html"));
        assert!(output.contains("hello"));
    }
}
