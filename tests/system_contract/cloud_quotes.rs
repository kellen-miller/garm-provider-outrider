use super::*;

impl Drop for SystemScenario {
    fn drop(&mut self) {
        self.aws.task.abort();
        let _result = self.daemon.kill();
        let _result = self.daemon.wait();
    }
}

pub(super) async fn quoted_catalog_offer(
    backend: &dyn CloudCatalog,
    cloud: &str,
    account: &str,
    location: &str,
    profile: outrider::config::RunnerProfile,
    deadline: tokio::time::Instant,
) -> Result<outrider::policy::CatalogOffer, Box<dyn std::error::Error>> {
    let mut offers = backend
        .catalog(
            CatalogQuery {
                clouds: vec![cloud.to_owned()],
                accounts: vec![account.to_owned()],
                locations: vec![location.to_owned()],
                profile: profile.clone(),
            },
            deadline,
        )
        .await?
        .offers;
    let mut offer = offers
        .drain(..)
        .next()
        .ok_or_else(|| format!("{cloud} fixture did not discover a catalog offer"))?;
    let quote = backend
        .quote(
            &offer,
            Duration::from_secs(profile.cost_horizon_seconds),
            Duration::from_secs(600),
            deadline,
        )
        .await?;
    quote.estimated_total(profile.cost_horizon_seconds)?;
    offer.candidate.price = Some(quote);
    offer
        .candidate
        .meets(&profile)
        .map_err(|reason| format!("{cloud} fixture offer is ineligible: {reason}"))?;
    Ok(offer)
}
