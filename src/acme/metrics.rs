//! Prometheus series for managed certificates — enough for the standard
//! "expires in < 7 d" and "renewal failing" alerts.

use std::sync::Arc;

use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};

use super::CertState;

pub struct AcmeMetrics {
    /// `featherbit_acme_cert_not_after_timestamp_seconds{cert_id}` (0 for placeholders).
    pub not_after: IntGaugeVec,
    /// `featherbit_acme_cert_state{cert_id,state}` — 1 for the current state, 0 otherwise.
    pub state: IntGaugeVec,
    /// `featherbit_acme_renewals_total{cert_id,result="success"|"failure"}`.
    pub renewals: IntCounterVec,
    /// `featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id}`.
    pub last_attempt: IntGaugeVec,
}

impl AcmeMetrics {
    /// Creates the collectors and registers them on `registry`.
    /// Returns an error if any collector is already registered on this registry
    /// (programming error; each registry must be used for at most one `register` call).
    pub fn register(registry: &Registry) -> Result<Arc<Self>, prometheus::Error> {
        let not_after = IntGaugeVec::new(
            Opts::new(
                "featherbit_acme_cert_not_after_timestamp_seconds",
                "Expiry of the served managed certificate (unix seconds; 0 = placeholder)",
            ),
            &["cert_id"],
        )
        .unwrap();
        let state = IntGaugeVec::new(
            Opts::new(
                "featherbit_acme_cert_state",
                "Current state of a managed certificate (1 = current)",
            ),
            &["cert_id", "state"],
        )
        .unwrap();
        let renewals = IntCounterVec::new(
            Opts::new(
                "featherbit_acme_renewals_total",
                "ACME issuance/renewal attempts by outcome",
            ),
            &["cert_id", "result"],
        )
        .unwrap();
        let last_attempt = IntGaugeVec::new(
            Opts::new(
                "featherbit_acme_last_renewal_attempt_timestamp_seconds",
                "Unix time of the last issuance attempt",
            ),
            &["cert_id"],
        )
        .unwrap();
        for c in [
            Box::new(not_after.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(state.clone()),
            Box::new(renewals.clone()),
            Box::new(last_attempt.clone()),
        ] {
            registry.register(c)?;
        }
        Ok(Arc::new(Self {
            not_after,
            state,
            renewals,
            last_attempt,
        }))
    }

    pub fn observe(&self, cert_id: &str, state: CertState, not_after: i64) {
        self.not_after.with_label_values(&[cert_id]).set(not_after);
        for s in CertState::ALL {
            self.state
                .with_label_values(&[cert_id, s.as_str()])
                .set(i64::from(s == state));
        }
    }

    pub fn attempt(&self, cert_id: &str, success: bool, at: i64) {
        self.renewals
            .with_label_values(&[cert_id, if success { "success" } else { "failure" }])
            .inc();
        self.last_attempt.with_label_values(&[cert_id]).set(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{Encoder, TextEncoder};

    fn render(r: &prometheus::Registry) -> String {
        let mut buf = Vec::new();
        TextEncoder::new().encode(&r.gather(), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn series_render_with_expected_names_and_labels() {
        let registry = prometheus::Registry::new();
        let m = AcmeMetrics::register(&registry).unwrap();
        m.observe("a.example.com", CertState::Issued, 1_800_000_000);
        m.attempt("a.example.com", true, 1_700_000_000);
        m.attempt("a.example.com", false, 1_700_000_100);
        let out = render(&registry);
        assert!(out.contains("featherbit_acme_cert_not_after_timestamp_seconds{cert_id=\"a.example.com\"} 1800000000"), "{out}");
        assert!(out
            .contains("featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"issued\"} 1"));
        assert!(out.contains(
            "featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"placeholder\"} 0"
        ));
        assert!(out.contains(
            "featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"renewing\"} 0"
        ));
        assert!(out
            .contains("featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"failed\"} 0"));
        assert!(out.contains(
            "featherbit_acme_renewals_total{cert_id=\"a.example.com\",result=\"success\"} 1"
        ));
        assert!(out.contains(
            "featherbit_acme_renewals_total{cert_id=\"a.example.com\",result=\"failure\"} 1"
        ));
        assert!(out.contains("featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id=\"a.example.com\"} 1700000100"));
        // Placeholder reports 0 for not_after.
        m.observe("b.example.com", CertState::Placeholder, 0);
        assert!(render(&registry).contains(
            "featherbit_acme_cert_not_after_timestamp_seconds{cert_id=\"b.example.com\"} 0"
        ));
        // Registering twice on the same registry must fail.
        assert!(matches!(
            AcmeMetrics::register(&registry),
            Err(prometheus::Error::AlreadyReg)
        ));
    }
}
