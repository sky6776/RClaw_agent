use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex, RwLock,
};
use std::time::{Duration, Instant};

use super::types::{BackendError, BackendErrorKind, BackendId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Cooldown,
    QuotaBlocked,
    AuthBlocked,
}

impl HealthStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Cooldown => "cooldown",
            Self::QuotaBlocked => "quota-blocked",
            Self::AuthBlocked => "auth-blocked",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BackendStatus {
    pub id: BackendId,
    pub enabled: bool,
    pub health: HealthStatus,
    pub primary: bool,
}

#[derive(Debug)]
enum HealthState {
    Healthy,
    CooldownUntil(Instant),
    QuotaBlocked,
    AuthBlocked,
}

#[derive(Debug)]
struct BackendRuntime {
    enabled: AtomicBool,
    consecutive_failures: AtomicUsize,
    health: Mutex<HealthState>,
}

impl BackendRuntime {
    fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            consecutive_failures: AtomicUsize::new(0),
            health: Mutex::new(HealthState::Healthy),
        }
    }

    fn reset(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self.health.lock().expect("web_search health 锁中毒") = HealthState::Healthy;
    }

    fn health_status(&self) -> HealthStatus {
        let mut health = self.health.lock().expect("web_search health 锁中毒");
        match &*health {
            HealthState::CooldownUntil(until) if *until <= Instant::now() => {
                *health = HealthState::Healthy;
                self.consecutive_failures.store(0, Ordering::Relaxed);
                HealthStatus::Healthy
            }
            HealthState::Healthy => HealthStatus::Healthy,
            HealthState::CooldownUntil(_) => HealthStatus::Cooldown,
            HealthState::QuotaBlocked => HealthStatus::QuotaBlocked,
            HealthState::AuthBlocked => HealthStatus::AuthBlocked,
        }
    }
}

/// Shared control-plane state used by the router and slash commands.
#[derive(Debug)]
pub struct WebSearchControl {
    order: RwLock<Vec<BackendId>>,
    backends: HashMap<BackendId, BackendRuntime>,
    fallback_enabled: AtomicBool,
    failure_threshold: usize,
    cooldown: Duration,
}

impl WebSearchControl {
    pub fn new(
        order: Vec<BackendId>,
        configured_enabled: &[(BackendId, bool)],
        fallback_enabled: bool,
        failure_threshold: usize,
        cooldown: Duration,
    ) -> Result<Self, String> {
        if order.is_empty() {
            return Err("web_search order 不能为空".into());
        }
        let mut configured = HashSet::new();
        for (id, _) in configured_enabled {
            if !configured.insert(*id) {
                return Err(format!("web_search provider 配置重复：{id}"));
            }
        }
        let mut seen = HashSet::new();
        for id in &order {
            if !configured.contains(id) {
                return Err(format!("web_search order 包含未配置的方式：{id}"));
            }
            if !seen.insert(*id) {
                return Err(format!("web_search order 包含重复方式：{id}"));
            }
        }
        if configured.len() != order.len() {
            let missing = configured
                .difference(&seen)
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("web_search order 缺少已配置方式：{missing}"));
        }
        let backends = configured_enabled
            .iter()
            .map(|(id, enabled)| (*id, BackendRuntime::new(*enabled)))
            .collect();
        Ok(Self {
            order: RwLock::new(order),
            backends,
            fallback_enabled: AtomicBool::new(fallback_enabled),
            failure_threshold: failure_threshold.max(1),
            cooldown,
        })
    }

    pub fn order(&self) -> Vec<BackendId> {
        self.order.read().expect("web_search order 锁中毒").clone()
    }

    pub fn move_to_front(&self, id: BackendId) -> Result<Vec<BackendId>, String> {
        if !self.backends.contains_key(&id) {
            return Err(format!("搜索方式 {id} 未配置"));
        }
        let mut order = self.order.write().expect("web_search order 锁中毒");
        if let Some(index) = order.iter().position(|current| *current == id) {
            order.remove(index);
        }
        order.insert(0, id);
        tracing::info!(provider = id.as_str(), "web_search 控制面已调整搜索优先级");
        Ok(order.clone())
    }

    pub fn is_configured(&self, id: BackendId) -> bool {
        self.backends.contains_key(&id)
    }

    pub fn set_enabled(&self, id: BackendId, enabled: bool) -> Result<(), String> {
        let state = self
            .backends
            .get(&id)
            .ok_or_else(|| format!("搜索方式 {id} 未配置"))?;
        state.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            state.reset();
        }
        tracing::info!(
            provider = id.as_str(),
            enabled,
            "web_search 控制面已修改手动启用状态"
        );
        Ok(())
    }

    pub fn is_enabled(&self, id: BackendId) -> bool {
        self.backends
            .get(&id)
            .is_some_and(|state| state.enabled.load(Ordering::Relaxed))
    }

    /// 返回独立工具拒绝调用当前方式的原因；聚合路由使用 `eligible_order` 自动跳过。
    pub fn unavailable_reason(&self, id: BackendId) -> Option<String> {
        let state = self.backends.get(&id)?;
        if !state.enabled.load(Ordering::Relaxed) {
            return Some(format!(
                "{} 已禁用；需要恢复时使用 /search en {}",
                id.label(),
                id.as_str()
            ));
        }
        let health = state.health_status();
        if health != HealthStatus::Healthy {
            return Some(format!(
                "{} 当前不可用（{}）；额度或凭据恢复后使用 /search en {} 清除阻塞状态",
                id.label(),
                health.label(),
                id.as_str()
            ));
        }
        None
    }

    pub fn fallback_enabled(&self) -> bool {
        self.fallback_enabled.load(Ordering::Relaxed)
    }

    pub fn set_fallback_enabled(&self, enabled: bool) {
        self.fallback_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn eligible_order(&self) -> Vec<BackendId> {
        self.order()
            .into_iter()
            .filter(|id| {
                self.backends.get(id).is_some_and(|state| {
                    state.enabled.load(Ordering::Relaxed)
                        && state.health_status() == HealthStatus::Healthy
                })
            })
            .collect()
    }

    pub fn statuses(&self) -> Vec<BackendStatus> {
        let order = self.order();
        let primary = order.iter().copied().find(|id| {
            self.backends.get(id).is_some_and(|state| {
                state.enabled.load(Ordering::Relaxed)
                    && state.health_status() == HealthStatus::Healthy
            })
        });
        order
            .into_iter()
            .filter_map(|id| {
                self.backends.get(&id).map(|state| BackendStatus {
                    id,
                    enabled: state.enabled.load(Ordering::Relaxed),
                    health: state.health_status(),
                    primary: primary == Some(id),
                })
            })
            .collect()
    }

    pub fn record_success(&self, id: BackendId) {
        if let Some(state) = self.backends.get(&id) {
            let mut health = state.health.lock().expect("web_search health 锁中毒");
            match &*health {
                // 额度/鉴权阻塞只能由显式 `/search en` 清除。并发中的迟到成功响应
                // 不能覆盖已经观测到的终止性错误，否则会继续消耗已失效来源。
                HealthState::QuotaBlocked => {
                    tracing::debug!(
                        provider = id.as_str(),
                        health = "quota-blocked",
                        "web_search 忽略迟到的成功响应，保留终止阻塞状态"
                    );
                }
                HealthState::AuthBlocked => {
                    tracing::debug!(
                        provider = id.as_str(),
                        health = "auth-blocked",
                        "web_search 忽略迟到的成功响应，保留终止阻塞状态"
                    );
                }
                HealthState::Healthy => {
                    state.consecutive_failures.store(0, Ordering::Relaxed);
                }
                HealthState::CooldownUntil(_) => {
                    *health = HealthState::Healthy;
                    state.consecutive_failures.store(0, Ordering::Relaxed);
                    tracing::info!(
                        provider = id.as_str(),
                        previous = "cooldown",
                        "web_search 搜索方式探测成功，健康状态已恢复"
                    );
                }
            }
        }
    }

    pub fn record_error(&self, id: BackendId, error: &BackendError) {
        let Some(state) = self.backends.get(&id) else {
            return;
        };
        tracing::warn!(
            provider = id.as_str(),
            error_kind = ?error.kind,
            error = %rclaw_store::redact_sensitive_text(&error.concise()),
            "web_search 搜索方式调用失败"
        );
        match error.kind {
            BackendErrorKind::Authentication => {
                *state.health.lock().expect("web_search health 锁中毒") = HealthState::AuthBlocked;
                tracing::warn!(
                    provider = id.as_str(),
                    "web_search 搜索方式因鉴权错误进入阻塞状态，需 /search en 清除"
                );
            }
            BackendErrorKind::QuotaExhausted => {
                *state.health.lock().expect("web_search health 锁中毒") = HealthState::QuotaBlocked;
                tracing::warn!(
                    provider = id.as_str(),
                    "web_search 搜索方式因额度耗尽进入阻塞状态，需 /search en 清除"
                );
            }
            BackendErrorKind::Transient | BackendErrorKind::RateLimited => {
                let mut health = state.health.lock().expect("web_search health 锁中毒");
                if matches!(
                    &*health,
                    HealthState::QuotaBlocked | HealthState::AuthBlocked
                ) {
                    tracing::debug!(
                        provider = id.as_str(),
                        error_kind = ?error.kind,
                        "web_search 忽略迟到的非终止错误，保留终止阻塞状态"
                    );
                    return;
                }
                let failures = state.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= self.failure_threshold {
                    *health = HealthState::CooldownUntil(Instant::now() + self.cooldown);
                    tracing::warn!(
                        provider = id.as_str(),
                        failures,
                        cooldown_secs = self.cooldown.as_secs(),
                        "web_search 搜索方式达到失败阈值，熔断进入冷却"
                    );
                }
            }
            BackendErrorKind::InvalidRequest
            | BackendErrorKind::EmptyOrInsufficient
            | BackendErrorKind::Permanent => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> WebSearchControl {
        WebSearchControl::new(
            vec![BackendId::DoubaoCustom, BackendId::Tavily],
            &[(BackendId::DoubaoCustom, true), (BackendId::Tavily, true)],
            true,
            2,
            Duration::from_secs(60),
        )
        .unwrap()
    }

    #[test]
    fn disable_keeps_order_and_enable_resets_health() {
        let control = control();
        control.record_error(BackendId::DoubaoCustom, &BackendError::quota("exhausted"));
        assert_eq!(control.statuses()[0].health, HealthStatus::QuotaBlocked);
        control.set_enabled(BackendId::DoubaoCustom, false).unwrap();
        assert_eq!(control.eligible_order(), vec![BackendId::Tavily]);
        control.set_enabled(BackendId::DoubaoCustom, true).unwrap();
        assert_eq!(
            control.eligible_order(),
            vec![BackendId::DoubaoCustom, BackendId::Tavily]
        );
    }

    #[test]
    fn transient_failures_open_circuit_at_threshold() {
        let control = control();
        let error = BackendError::transient("timeout");
        control.record_error(BackendId::Tavily, &error);
        assert!(control.eligible_order().contains(&BackendId::Tavily));
        control.record_error(BackendId::Tavily, &error);
        assert!(!control.eligible_order().contains(&BackendId::Tavily));
    }

    #[test]
    fn duplicate_configured_provider_is_rejected() {
        let result = WebSearchControl::new(
            vec![BackendId::Tavily],
            &[(BackendId::Tavily, true), (BackendId::Tavily, false)],
            true,
            2,
            Duration::from_secs(60),
        );
        assert!(result.unwrap_err().contains("配置重复"));
    }

    #[test]
    fn late_success_does_not_clear_terminal_block() {
        let control = control();
        control.record_error(BackendId::Tavily, &BackendError::quota("exhausted"));
        control.record_success(BackendId::Tavily);
        assert_eq!(
            control.statuses()[1].health,
            HealthStatus::QuotaBlocked,
            "并发中的迟到成功不能覆盖额度阻塞"
        );
        control.set_enabled(BackendId::Tavily, true).unwrap();
        assert_eq!(control.statuses()[1].health, HealthStatus::Healthy);
    }

    #[test]
    fn late_transient_errors_do_not_downgrade_terminal_block() {
        let control = control();
        control.record_error(BackendId::Tavily, &BackendError::quota("exhausted"));
        control.record_error(BackendId::Tavily, &BackendError::transient("timeout one"));
        control.record_error(BackendId::Tavily, &BackendError::transient("timeout two"));
        assert_eq!(
            control.statuses()[1].health,
            HealthStatus::QuotaBlocked,
            "迟到的瞬态错误不能把额度阻塞降级为会自动恢复的 cooldown"
        );
    }
}
