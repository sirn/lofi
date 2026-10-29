//! Typed decision questions and answers, shared by the policy layer that
//! writes them and the transport that sends them.
//!
//! A decision model answers named questions about a state; it never
//! generates text. The wire format follows the systemone endpoint,
//! but the types carry no transport detail.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One question about the decision state.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionQuestion {
    /// A yes/no statement. The answer is the probability that it is true.
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// One option out of a named set. The criteria map every allowed
    /// answer to a description of when it applies.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// A position on an ordered scale. The criteria list the levels from
    /// lowest to highest.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// What counts as a yes and a no for a noul question.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// One typed answer, keyed by its question's name.
///
/// An answer type the reader does not know decodes as [`Self::Unknown`]
/// instead of failing the whole response, so one new answer shape cannot
/// hide the answers that did parse.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionAnswer {
    /// Probability of a yes answer, 0 to 1.
    Noul { noul: f64 },
    /// The selected option, its calibrated probabilities, and an overall
    /// confidence. The probabilities sum to 1, so a Choice nominates a
    /// winner even when nothing fits.
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// The probability-weighted position on the question's ordered scale.
    Score {
        score: f64,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    #[serde(other)]
    Unknown,
}

impl DecisionAnswer {
    /// The yes-probability of a noul answer.
    #[must_use]
    pub fn noul(&self) -> Option<f64> {
        match self {
            Self::Noul { noul } => Some(*noul),
            _ => None,
        }
    }

    /// The overall confidence of a choice or score answer. A noul answer
    /// is one probability and carries no separate confidence.
    #[must_use]
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Self::Choice { confidence, .. } | Self::Score { confidence, .. } => *confidence,
            _ => None,
        }
    }

    /// Whether the answer carries the numbers its type promises: noul and
    /// confidence values are probabilities, and a choice names an option.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let probability = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        match self {
            Self::Noul { noul } => probability(*noul),
            Self::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                probabilities.values().all(|v| probability(*v))
                    && confidence.is_none_or(probability)
                    && !choice.is_empty()
            }
            Self::Score {
                score,
                probabilities,
                confidence,
            } => {
                score.is_finite()
                    && probabilities.values().all(|v| probability(*v))
                    && confidence.is_none_or(probability)
            }
            Self::Unknown => false,
        }
    }
}
