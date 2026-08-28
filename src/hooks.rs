#![forbid(unsafe_code)]

use std::sync::Arc;

use serde_json::Value;

use crate::probe::ProductProbe;

type BindRawHook = Arc<dyn Fn(&str) -> String + Send + Sync>;
type AllowHook = Arc<dyn Fn(bool) -> bool + Send + Sync>;
type ExtraHealthHook = Arc<dyn Fn() -> Option<Value> + Send + Sync>;
type ReadyHook = Arc<dyn Fn() -> bool + Send + Sync>;

/// Product-specific overrides of shared sidecar behavior.
///
/// Implement this on a named type when the product has real policy. Unoverridden
/// methods keep the shared-library default (pass the env bind through, honor
/// `ALLOW_NON_LOOPBACK`, `/readyz` is ready, no extra health payload).
pub trait SidecarOverrides: ProductProbe + Send + Sync {
    /// Rewrite the bind string after env lookup. `raw` is already
    /// `BIND` or `identity.default_bind`.
    fn bind_raw(&self, raw: &str) -> String {
        raw.to_string()
    }

    /// Filter the env-derived allow-non-loopback flag. Return false to refuse
    /// a public bind even when the env flag is on.
    fn allow_non_loopback(&self, from_env: bool) -> bool {
        from_env
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultOverrides;

impl ProductProbe for DefaultOverrides {}
impl SidecarOverrides for DefaultOverrides {}

/// Closure bag for call sites that want a JS-style object of optional functions.
///
/// ```ignore
/// SidecarHooks::new()
///     .bind_raw(|raw| values.bind.clone())
///     .allow_non_loopback(|flag| flag && values.allow_non_loopback)
///     .ready(|| ping_product())
/// ```
#[derive(Clone, Default)]
pub struct SidecarHooks {
    bind_raw: Option<BindRawHook>,
    allow_non_loopback: Option<AllowHook>,
    extra_health: Option<ExtraHealthHook>,
    ready: Option<ReadyHook>,
}

impl SidecarHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind_raw(self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        Self {
            bind_raw: Some(Arc::new(f)),
            ..self
        }
    }

    pub fn allow_non_loopback(self, f: impl Fn(bool) -> bool + Send + Sync + 'static) -> Self {
        Self {
            allow_non_loopback: Some(Arc::new(f)),
            ..self
        }
    }

    pub fn extra_health(self, f: impl Fn() -> Option<Value> + Send + Sync + 'static) -> Self {
        Self {
            extra_health: Some(Arc::new(f)),
            ..self
        }
    }

    pub fn ready(self, f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            ready: Some(Arc::new(f)),
            ..self
        }
    }
}

impl ProductProbe for SidecarHooks {
    fn extra_health(&self) -> Option<Value> {
        self.extra_health.as_ref().and_then(|f| f())
    }

    fn ready(&self) -> bool {
        self.ready.as_ref().map(|f| f()).unwrap_or(true)
    }
}

impl SidecarOverrides for SidecarHooks {
    fn bind_raw(&self, raw: &str) -> String {
        match &self.bind_raw {
            Some(f) => f(raw),
            None => raw.to_string(),
        }
    }

    fn allow_non_loopback(&self, from_env: bool) -> bool {
        match &self.allow_non_loopback {
            Some(f) => f(from_env),
            None => from_env,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RefusePublicBind;

    impl ProductProbe for RefusePublicBind {
        fn ready(&self) -> bool {
            false
        }
    }

    impl SidecarOverrides for RefusePublicBind {
        fn allow_non_loopback(&self, _from_env: bool) -> bool {
            false
        }

        fn bind_raw(&self, _raw: &str) -> String {
            "127.0.0.1:19090".into()
        }
    }

    #[test]
    fn default_overrides_pass_values_through() {
        let hooks = DefaultOverrides;
        assert_eq!(hooks.bind_raw("127.0.0.1:9090"), "127.0.0.1:9090");
        assert!(hooks.allow_non_loopback(true));
        assert!(!hooks.allow_non_loopback(false));
        assert!(hooks.ready());
        assert!(hooks.extra_health().is_none());
    }

    #[test]
    fn trait_impl_can_replace_bind_and_readiness() {
        let hooks = RefusePublicBind;
        assert_eq!(hooks.bind_raw("0.0.0.0:9090"), "127.0.0.1:19090");
        assert!(!hooks.allow_non_loopback(true));
        assert!(!hooks.ready());
    }

    #[test]
    fn closure_bag_overrides_only_named_hooks() {
        let hooks = SidecarHooks::new()
            .bind_raw(|raw| format!("rewritten:{raw}"))
            .ready(|| false);
        assert_eq!(
            SidecarOverrides::bind_raw(&hooks, "127.0.0.1:1"),
            "rewritten:127.0.0.1:1"
        );
        assert!(SidecarOverrides::allow_non_loopback(&hooks, true));
        assert!(!ProductProbe::ready(&hooks));
        assert!(ProductProbe::extra_health(&hooks).is_none());
    }
}
