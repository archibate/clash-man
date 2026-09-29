//! Persistent routing policy, compiled into native core groups and a shared node provider.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml_ng::{Mapping, Value};

const PROVIDER: &str = "clash-man-nodes";
pub const DEFAULT_TEST_URL: &str = "https://www.gstatic.com/generate_204";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutoPolicy {
    pub enabled: bool,
    pub group: String,
    pub preferred: Vec<String>,
    pub interval_seconds: u64,
    pub test_url: String,
}

impl AutoPolicy {
    pub fn automatic_group(&self) -> String {
        format!("{} Auto", self.group)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.group.trim().is_empty() || self.group == "GLOBAL" {
            return Err("choose a named routing group other than GLOBAL".into());
        }
        if !(5..=86_400).contains(&self.interval_seconds) {
            return Err("health-check interval must be between 5s and 1d".into());
        }
        let url = url::Url::parse(&self.test_url).map_err(|_| "invalid health-check URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("health-check URL must use HTTP or HTTPS".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("health-check URL must not contain credentials".into());
        }
        Ok(())
    }
}

pub struct Rendered {
    pub config: String,
    pub provider: Option<String>,
}

/// Keep machine-local access and network settings out of the subscription's control.
pub fn preserve_host_settings(source: &str, current: Option<&str>) -> Result<String, String> {
    let Some(current) = current else {
        return Ok(source.to_owned());
    };
    let incoming = mapping(source)?;
    let original = incoming.clone();
    let mut new = mapping(current)?;
    // Subscriptions own routing only. New core settings remain host-owned by default.
    for key in [
        "proxies",
        "proxy-groups",
        "rules",
        "proxy-providers",
        "rule-providers",
        "sub-rules",
    ] {
        if let Some(value) = incoming.get(key) {
            new.insert(key.into(), value.clone());
        } else {
            new.remove(key);
        }
    }
    if new == original {
        Ok(source.to_owned())
    } else {
        yaml(&new)
    }
}

pub fn render(
    source: &str,
    policy: Option<&AutoPolicy>,
    provider_path: &Path,
) -> Result<Rendered, String> {
    let Some(policy) = policy.filter(|policy| policy.enabled) else {
        return Ok(Rendered {
            config: source.to_owned(),
            provider: None,
        });
    };
    policy.validate()?;
    let mut root = mapping(source)?;
    let mut nodes = root
        .get("proxies")
        .and_then(Value::as_sequence)
        .cloned()
        .filter(|nodes| !nodes.is_empty())
        .ok_or("automatic routing requires a subscription with inline proxies")?;
    let mut names = BTreeSet::new();
    for node in &nodes {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .ok_or("proxy has no name")?;
        if !names.insert(name.to_owned()) {
            return Err(format!("duplicate proxy name: {name}"));
        }
    }
    for node in &nodes {
        if node
            .get("dialer-proxy")
            .and_then(Value::as_str)
            .is_some_and(|name| names.contains(name))
        {
            return Err("automatic routing does not support chained inline proxies".into());
        }
    }
    // Node names referenced directly by a rule need a static adapter; do not silently break them.
    if root
        .get("rules")
        .and_then(Value::as_sequence)
        .is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule.as_str().is_some_and(|rule| {
                    rule.split(',')
                        .rev()
                        .find(|part| *part != "no-resolve")
                        .is_some_and(|target| names.contains(target.trim()))
                })
            })
        })
    {
        return Err(
            "automatic routing requires rules to target groups, not individual nodes".into(),
        );
    }
    let mut groups = root
        .get("proxy-groups")
        .and_then(Value::as_sequence)
        .cloned()
        .ok_or("configuration has no proxy groups")?;
    let auto_name = policy.automatic_group();
    if groups
        .iter()
        .any(|group| group.get("name").and_then(Value::as_str) == Some(&auto_name))
    {
        return Err(format!(
            "subscription already contains reserved group {auto_name}"
        ));
    }
    let target = groups
        .iter()
        .position(|group| group.get("name").and_then(Value::as_str) == Some(&policy.group))
        .ok_or_else(|| {
            format!(
                "routing group {} is absent from the subscription",
                policy.group
            )
        })?;
    let pool = string_list(groups[target].get("proxies"))?;
    if groups[target].get("use").is_some() || groups[target].get("filter").is_some() {
        return Err(
            "automatic routing does not support a target group mixing inline nodes and providers"
                .into(),
        );
    }
    if pool.is_empty() || pool.iter().any(|name| !names.contains(name)) {
        return Err("automatic routing requires a group containing only inline nodes".into());
    }
    let priorities: BTreeMap<_, _> = policy
        .preferred
        .iter()
        .enumerate()
        .rev()
        .map(|(rank, name)| (name.as_str(), rank))
        .collect();
    nodes.sort_by_cached_key(|node| {
        priorities
            .get(node["name"].as_str().unwrap_or_default())
            .copied()
            .unwrap_or(usize::MAX)
    });
    let mut providers = match root.get("proxy-providers") {
        None | Some(Value::Null) => Mapping::new(),
        Some(value) => value
            .as_mapping()
            .cloned()
            .ok_or("proxy-providers is not a mapping")?,
    };
    if providers.contains_key(PROVIDER) {
        return Err(format!(
            "subscription already contains reserved provider {PROVIDER}"
        ));
    }
    providers.insert(
        PROVIDER.into(),
        value(json!({
            "type": "file", "path": provider_path,
            "health-check": {"enable": true, "url": policy.test_url,
                             "interval": policy.interval_seconds, "lazy": false}
        })),
    );
    for (index, group) in groups.iter_mut().enumerate() {
        if index == target {
            let group = group
                .as_mapping_mut()
                .ok_or("proxy group is not a mapping")?;
            group.insert("type".into(), "select".into());
            group.insert("proxies".into(), value(json!([auto_name])));
            continue;
        }
        let direct = string_list(group.get("proxies"))?;
        let included: Vec<_> = direct
            .iter()
            .filter(|name| names.contains(*name))
            .cloned()
            .collect();
        if included.is_empty() {
            continue;
        }
        let included_set: BTreeSet<_> = included.iter().map(String::as_str).collect();
        let preserves_order = nodes
            .iter()
            .filter_map(|node| node.get("name").and_then(Value::as_str))
            .filter(|name| included_set.contains(name))
            .eq(included.iter().map(String::as_str));
        let group_type = group.get("type").and_then(Value::as_str);
        if !(matches!(group_type, Some("select" | "url-test"))
            || group_type == Some("fallback") && preserves_order)
            || included.len() != direct.len()
        {
            return Err("cannot preserve ordering of mixed or order-sensitive groups; remove conflicting preferences or use node-only select/url-test groups".into());
        }
        if group.get("use").is_some() || group.get("filter").is_some() {
            return Err(
                "cannot safely convert a group mixing inline nodes with provider filters".into(),
            );
        }
        let group = group
            .as_mapping_mut()
            .ok_or("proxy group is not a mapping")?;
        let retained: Vec<_> = direct
            .into_iter()
            .filter(|name| !names.contains(name))
            .collect();
        group.insert("proxies".into(), value(json!(retained)));
        group.insert("use".into(), value(json!([PROVIDER])));
        group.insert("filter".into(), exact_filter(&included).into());
    }
    groups.insert(
        target + 1,
        value(json!({
            "name": auto_name, "type": "fallback", "use": [PROVIDER],
            "filter": exact_filter(&pool), "url": policy.test_url,
            "interval": policy.interval_seconds, "lazy": false
        })),
    );
    root.remove("proxies");
    root.insert("proxy-providers".into(), Value::Mapping(providers));
    root.insert("proxy-groups".into(), Value::Sequence(groups));
    Ok(Rendered {
        config: yaml(&root)?,
        provider: Some(yaml(&value(json!({"proxies": nodes})))?),
    })
}

fn string_list(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Sequence(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "proxy list contains a non-string entry".into())
            })
            .collect(),
        _ => Err("proxy list is not a sequence".into()),
    }
}

fn exact_filter(names: &[String]) -> String {
    let alternatives: Vec<_> = names
        .iter()
        .map(|name| {
            let mut escaped = String::new();
            for ch in name.chars() {
                if ch == '`' {
                    escaped.push_str("\\x60");
                    continue;
                }
                if "\\.+*?()|[]{}^$".contains(ch) {
                    escaped.push('\\');
                }
                escaped.push(ch);
            }
            escaped
        })
        .collect();
    format!("^({})$", alternatives.join("|"))
}

fn mapping(text: &str) -> Result<Mapping, String> {
    serde_yaml_ng::from_str(text).map_err(|_| "configuration must be a YAML mapping".into())
}

fn yaml(value: &impl Serialize) -> Result<String, String> {
    serde_yaml_ng::to_string(value).map_err(|_| "could not serialize routing configuration".into())
}

fn value(value: serde_json::Value) -> Value {
    serde_yaml_ng::to_value(value).expect("JSON values serialize as YAML")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "proxies:\n  - {name: 'US.01', type: ss}\n  - {name: 'JP+02', type: ss}\nproxy-groups:\n  - {name: Proxy, type: select, proxies: ['US.01', 'JP+02']}\n  - {name: Japan, type: url-test, proxies: ['JP+02']}\n  - {name: Domestic, type: select, proxies: [DIRECT, Proxy]}\nrules: ['MATCH,Proxy']\n";

    fn policy() -> AutoPolicy {
        AutoPolicy {
            enabled: true,
            group: "Proxy".into(),
            preferred: vec!["JP+02".into()],
            interval_seconds: 30,
            test_url: DEFAULT_TEST_URL.into(),
        }
    }

    #[test]
    fn subscriptions_cannot_inject_host_settings() {
        let source = "proxies: []\nlisteners: [{type: http, port: 8080}]\ndns: {listen: '0.0.0.0:53'}\nfuture-core-listener: 9999\n";
        let output =
            preserve_host_settings(source, Some("port: 7890\ndns: {enable: false}\n")).unwrap();
        let config = mapping(&output).unwrap();
        assert!(!config.contains_key("listeners"));
        assert!(!config.contains_key("future-core-listener"));
        assert_eq!(config["dns"]["enable"], false);
        assert_eq!(config["port"], 7890);
    }

    #[test]
    fn backticks_are_encoded_before_core_filter_splitting() {
        let filter = exact_filter(&["a`b".into()]);
        assert!(!filter.contains('`'));
        assert_eq!(filter, r"^(a\x60b)$");
    }

    #[test]
    fn shares_node_instances_and_preserves_group_membership() {
        let rendered = render(SOURCE, Some(&policy()), Path::new("/config/nodes.yaml")).unwrap();
        let config = mapping(&rendered.config).unwrap();
        assert!(!config.contains_key("proxies"));
        let groups = config["proxy-groups"].as_sequence().unwrap();
        assert_eq!(groups[0]["proxies"], value(json!(["Proxy Auto"])));
        assert_eq!(groups[1]["type"], "fallback");
        assert_eq!(groups[2]["filter"], "^(JP\\+02)$");
        assert_eq!(groups[3]["proxies"], value(json!(["DIRECT", "Proxy"])));
        assert_eq!(config["rules"], value(json!(["MATCH,Proxy"])));
        let provider = mapping(rendered.provider.as_ref().unwrap()).unwrap();
        assert_eq!(provider["proxies"][0]["name"], "JP+02");
        assert_eq!(provider["proxies"].as_sequence().unwrap().len(), 2);
        assert_eq!(
            config["proxy-providers"][PROVIDER]["health-check"]["lazy"],
            false
        );
    }

    #[test]
    fn disappearing_preference_does_not_remove_new_nodes() {
        let mut policy = policy();
        policy.preferred = vec!["removed".into()];
        let output = render(SOURCE, Some(&policy), Path::new("nodes.yaml")).unwrap();
        let nodes = mapping(output.provider.as_ref().unwrap()).unwrap();
        assert_eq!(nodes["proxies"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn disabled_mode_restores_source_without_generated_groups() {
        let mut policy = policy();
        policy.enabled = false;
        let output = render(SOURCE, Some(&policy), Path::new("nodes.yaml")).unwrap();
        assert_eq!(output.config, SOURCE);
        assert!(output.provider.is_none());
    }

    #[test]
    fn rejects_missing_group_and_bad_intervals() {
        let mut p = policy();
        p.group = "missing".into();
        assert!(render(SOURCE, Some(&p), Path::new("nodes.yaml")).is_err());
        p.interval_seconds = 0;
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_order_sensitive_or_mixed_groups_instead_of_changing_routes() {
        for extra in [
            "  - {name: Other, type: fallback, proxies: ['US.01', DIRECT]}\n",
            "  - {name: Other, type: relay, proxies: ['US.01', 'JP+02']}\n",
            "  - {name: Other, type: select, proxies: ['US.01', DIRECT]}\n",
        ] {
            let source = SOURCE.replace("rules:", &format!("{extra}rules:"));
            assert!(render(&source, Some(&policy()), Path::new("nodes.yaml")).is_err());
        }
        let mixed = SOURCE.replace(
            "name: Proxy, type: select,",
            "name: Proxy, type: select, use: [other],",
        );
        assert!(render(&mixed, Some(&policy()), Path::new("nodes.yaml")).is_err());
    }

    #[test]
    fn preserves_existing_fallback_order_and_rejects_conflicting_preferences() {
        let source = SOURCE.replace(
            "type: url-test, proxies: ['JP+02']",
            "type: fallback, proxies: ['US.01', 'JP+02']",
        );
        let mut policy = policy();
        policy.preferred.clear();
        assert!(render(&source, Some(&policy), Path::new("nodes.yaml")).is_ok());
        policy.preferred = vec!["JP+02".into()];
        assert!(render(&source, Some(&policy), Path::new("nodes.yaml")).is_err());
    }

    #[test]
    fn preserves_target_group_behavior_flags() {
        let source = SOURCE.replace(
            "name: Proxy, type: select,",
            "name: Proxy, type: select, disable-udp: true,",
        );
        let output = render(&source, Some(&policy()), Path::new("nodes.yaml")).unwrap();
        assert_eq!(
            mapping(&output.config).unwrap()["proxy-groups"][0]["disable-udp"],
            true
        );
    }

    #[test]
    fn subscription_cannot_replace_host_listener_or_secret() {
        let output = preserve_host_settings(
            "port: 9999\nsecret: other\nexternal-controller: 0.0.0.0:9090\nproxies: []\n",
            Some("port: 7890\nsecret: private\nmode: rule\nproxies: []\n"),
        )
        .unwrap();
        let config = mapping(&output).unwrap();
        assert_eq!(config["port"], 7890);
        assert_eq!(config["secret"], "private");
        assert_eq!(config["mode"], "rule");
        assert!(!config.contains_key("external-controller"));
    }
}
