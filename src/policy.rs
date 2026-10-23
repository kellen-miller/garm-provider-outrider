mod candidates;
mod pricing;
mod ranking;
mod strategy;

pub use candidates::{
    AwsLaunchFacts, AzureLaunchFacts, Candidate, CatalogOffer, GcpLaunchFacts, NativeCapacityHint,
    ProviderLaunchFacts, ProviderSelection,
};
pub use pricing::{
    AppliedEffectivePrice, ExplanationGcpLicense, ExplanationLaunchFacts, FinancialPriceScope,
    ObservationAges, ObservationWindow, PriceComponent, PriceQuote,
};
pub use ranking::{CandidateExplanation, ScoreTerm, rank};
pub use strategy::{
    MissingDataBehavior, Policy, PolicyAttempt, PolicyDefinition, PolicyEvaluation, RankingResult,
    WeightedPolicy, evaluate_configured, rank_configured, validate_fallback_graph,
};

use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("required signal is missing: {0}")]
    MissingData(&'static str),
    #[error("cost arithmetic overflow")]
    CostOverflow,
    #[error("policy weights and scales are invalid")]
    InvalidWeightedPolicy,
    #[error("only USD price quotes can be compared")]
    UnsupportedCurrency,
    #[error("price components cannot be negative")]
    NegativePrice,
    #[error("candidate observation data is invalid")]
    InvalidObservation,
    #[error("no eligible candidates")]
    NoEligibleCandidates,
    #[error("fallback policy graph contains a cycle")]
    FallbackCycle,
}
