use super::{
    AccessTokenCredentials, BILLING_ORIGIN, BTreeSet, CLOUD_PLATFORM_SCOPE, COMPUTE_ORIGIN,
    CloudError, ComputeList, Decimal, Deserialize, Digest, GcpBackend, GcpCatalogBinding,
    GcpChildConfiguration, GcpCredentialsFile, GcpImage, GcpImageBinding, GcpLicenseBasis,
    GcpSettings, Instant, MAX_API_RESPONSE_BYTES, PathBuf, ProviderBinding, Sha256, failure,
    is_sha256, timeout_at,
};
use reqwest::Url;

impl GcpBackend {
    /// Builds runtime settings from the central catalog and qualified provider binding.
    ///
    /// # Errors
    /// Returns an error when the source facts or provider configuration are incomplete.
    pub fn settings_from_bindings(
        catalog: &GcpCatalogBinding,
        provider: &ProviderBinding,
    ) -> Result<GcpSettings, CloudError> {
        if provider.id != catalog.provider_binding_id
            || provider.cloud != "gcp"
            || provider.account != catalog.project_id
            || provider.location != catalog.zone
            || catalog.credential_reference
                != format!("provider-binding:{}:configuration", provider.id)
            || catalog.id.is_empty()
            || catalog.project_id.is_empty()
            || catalog.zone.is_empty()
            || catalog.pricing_location.is_empty()
            || catalog.images.is_empty()
            || catalog.storage_classes.is_empty()
            || catalog
                .storage_billing_skus
                .keys()
                .any(|key| !catalog.storage_classes.contains_key(key))
            || catalog.disk_performance.iter().any(|(key, facts)| {
                !catalog.storage_classes.contains_key(key)
                    || facts.source.is_empty()
                    || facts.maximum_iops == 0
                    || facts.maximum_throughput_mib_s <= Decimal::ZERO
                    || facts.base_throughput_mib_s < Decimal::ZERO
                    || facts.throughput_per_gib_mib_s < Decimal::ZERO
            })
            || catalog
                .external_ip_billing_sku
                .as_ref()
                .is_some_and(String::is_empty)
            || catalog.accelerator_facts.iter().any(|(name, facts)| {
                name.is_empty()
                    || facts.vendor.is_empty()
                    || facts.model.is_empty()
                    || facts.memory_gib <= Decimal::ZERO
                    || facts.spot_billing_sku.is_empty()
                    || facts.source.is_empty()
            })
            || catalog.images.iter().any(|(alias, image)| {
                alias.is_empty()
                    || image.image_id.is_empty()
                    || image.os.is_empty()
                    || image.os_source.is_empty()
                    || image.architecture.is_empty()
                    || image.capabilities_source.is_empty()
                    || image.license_facts.is_empty() == image.license_free_source.is_none()
                    || image
                        .license_free_source
                        .as_ref()
                        .is_some_and(String::is_empty)
                    || image.license_facts.iter().any(|(license, facts)| {
                        license.is_empty()
                            || facts.source.is_empty()
                            || match facts.basis {
                                GcpLicenseBasis::Included => facts.billing_sku.is_some(),
                                _ => facts.billing_sku.as_ref().is_none_or(String::is_empty),
                            }
                    })
            })
            || catalog.machine_facts.iter().any(|(name, facts)| {
                name.is_empty()
                    || facts.source.is_empty()
                    || facts
                        .architecture
                        .as_deref()
                        .is_some_and(|architecture| !matches!(architecture, "amd64" | "arm64"))
                    || facts.bundled_local_ssd_gib.is_some()
                        != facts.local_ssd_billing_sku.is_some()
                    || facts.bundled_local_ssd_gib.is_some()
                        != facts.bundled_local_ssd_partitions.is_some()
                    || facts.bundled_local_ssd_gib.is_some()
                        != facts.local_ssd_billing_quantity.is_some()
                    || facts.max_pd_iops.is_some() != facts.max_pd_throughput_mib_s.is_some()
                    || facts
                        .max_pd_throughput_mib_s
                        .is_some_and(|value| value <= Decimal::ZERO)
                    || facts
                        .shared_cpu_billing_sku
                        .as_ref()
                        .is_some_and(String::is_empty)
                    || facts
                        .local_ssd_billing_sku
                        .as_ref()
                        .is_some_and(String::is_empty)
            })
        {
            return Err(failure(
                "configuration",
                "GCP catalog binding does not match its pinned child-provider binding",
            ));
        }

        Ok(GcpSettings {
            catalog_id: catalog.id.clone(),
            project_id: catalog.project_id.clone(),
            zone: catalog.zone.clone(),
            pricing_location: catalog.pricing_location.clone(),
            images: catalog
                .images
                .iter()
                .map(|(name, image)| (name.clone(), image_from_binding(image)))
                .collect(),
            storage_classes: catalog.storage_classes.clone(),
            storage_billing_skus: catalog.storage_billing_skus.clone(),
            external_ip_billing_sku: catalog.external_ip_billing_sku.clone(),
            accelerator_facts: catalog.accelerator_facts.clone(),
            machine_facts: catalog.machine_facts.clone(),
            disk_performance: catalog.disk_performance.clone(),
            capacity_advice_enabled: catalog.capacity_advice_enabled,
            binding_id: provider.id.clone(),
            source_revision: provider.source_revision.clone(),
            binary_sha256: provider.executable_sha256.clone(),
            configuration_sha256: provider.configuration_sha256.clone(),
            purchasing_capabilities: provider
                .purchasing_capabilities
                .iter()
                .map(|capability| match capability {
                    crate::providers::PurchasingCapability::OnDemand => "on_demand".to_owned(),
                    crate::providers::PurchasingCapability::Spot => "spot".to_owned(),
                })
                .collect(),
            provider_configuration_path: provider.configuration_path.clone(),
            compute_endpoint: COMPUTE_ORIGIN.to_owned(),
            billing_endpoint: BILLING_ORIGIN.to_owned(),
        })
    }

    /// Loads the qualified child configuration and explicitly bound service-account key.
    ///
    /// # Errors
    /// Returns an error when credentials, target scope, or API setup is invalid.
    pub async fn connect(settings: GcpSettings, deadline: Instant) -> Result<Self, CloudError> {
        let (child, bytes) =
            read_child_configuration(&settings.provider_configuration_path, deadline).await?;
        validate_child_binding(&settings, &child, &bytes)?;
        if child.enable_regional_placement {
            return Err(failure(
                "unsupported",
                "fixed-zone GCP quotes cannot dispatch regional placement",
            ));
        }
        let credentials_path = PathBuf::from(&child.credentials_file);
        if !credentials_path.is_absolute() {
            return Err(failure(
                "configuration",
                "GCP credentials_file must be an absolute path",
            ));
        }
        let credential_bytes = timeout_at(deadline, tokio::fs::read(credentials_path))
            .await
            .map_err(|_| failure("timeout", "GCP credential loading timed out"))?
            .map_err(|_| failure("configuration", "GCP credentials_file could not be read"))?;
        let credential_value: serde_json::Value = serde_json::from_slice(&credential_bytes)
            .map_err(|_| failure("configuration", "GCP credentials_file is invalid JSON"))?;
        let credential_config: GcpCredentialsFile =
            serde_json::from_value(credential_value.clone()).map_err(|_| {
                failure(
                    "configuration",
                    "GCP credentials_file has no supported type",
                )
            })?;
        if credential_config.kind != "service_account" {
            return Err(failure(
                "unsupported",
                "GCP child credentials_file must contain a service_account key",
            ));
        }
        let credentials =
            google_cloud_auth::credentials::service_account::Builder::new(credential_value)
                .with_access_specifier(
                    google_cloud_auth::credentials::service_account::AccessSpecifier::from_scopes(
                        [CLOUD_PLATFORM_SCOPE],
                    ),
                )
                .build_access_token_credentials()
                .map_err(|_| {
                    failure(
                        "configuration",
                        "GCP service-account credentials are invalid",
                    )
                })?;
        timeout_at(deadline, credentials.access_token())
            .await
            .map_err(|_| failure("timeout", "GCP credential authentication timed out"))?
            .map_err(|_| {
                failure(
                    "authentication",
                    "GCP service-account authentication failed",
                )
            })?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| failure("configuration", "GCP HTTP client construction failed"))?;
        Self::with_clients(settings, client, credentials, deadline).await
    }

    /// Connects using explicit clients, primarily for source-shape contract tests.
    ///
    /// # Errors
    /// Returns an error when the child and catalog bindings do not agree.
    pub async fn with_clients(
        settings: GcpSettings,
        client: reqwest::Client,
        credentials: AccessTokenCredentials,
        deadline: Instant,
    ) -> Result<Self, CloudError> {
        let (child, bytes) =
            read_child_configuration(&settings.provider_configuration_path, deadline).await?;
        validate_child_binding(&settings, &child, &bytes)?;
        if child.enable_regional_placement {
            return Err(failure(
                "unsupported",
                "fixed-zone GCP quotes cannot dispatch regional placement",
            ));
        }
        if settings.project_id.is_empty()
            || settings.zone.is_empty()
            || settings.pricing_location.is_empty()
            || settings.binding_id.is_empty()
            || settings.catalog_id.is_empty()
            || settings.source_revision.is_empty()
            || !is_sha256(&settings.binary_sha256)
            || !is_sha256(&settings.configuration_sha256)
            || settings.images.is_empty()
            || settings.storage_classes.is_empty()
            || settings
                .storage_billing_skus
                .keys()
                .any(|key| !settings.storage_classes.contains_key(key))
            || settings
                .disk_performance
                .keys()
                .any(|key| !settings.storage_classes.contains_key(key))
        {
            return Err(failure(
                "configuration",
                "GCP catalog settings are incomplete",
            ));
        }
        validate_origin(&settings.compute_endpoint)?;
        validate_origin(&settings.billing_endpoint)?;
        if child.external_ip_access && settings.external_ip_billing_sku.is_none() {
            return Err(failure(
                "configuration",
                "external IPv4 is enabled but no verified GCP billing SKU is bound",
            ));
        }
        Ok(Self {
            settings,
            client,
            credentials,
            child,
        })
    }
    pub(super) async fn request_json<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        origin: &str,
        path: &str,
        deadline: Instant,
    ) -> Result<(T, Option<String>), CloudError> {
        let url = resource_url(origin, path)?;
        let token = timeout_at(deadline, self.credentials.access_token())
            .await
            .map_err(|_| failure("timeout", "GCP token refresh timed out"))?
            .map_err(|_| failure("authentication", "GCP token refresh failed"))?;
        let response = timeout_at(
            deadline,
            self.client.get(url).bearer_auth(&token.token).send(),
        )
        .await
        .map_err(|_| failure("timeout", "GCP read request timed out"))?
        .map_err(|_| failure("transport", "GCP read request failed"))?;
        let request_id = response
            .headers()
            .get("x-guploader-uploadid")
            .or_else(|| response.headers().get("x-goog-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !response.status().is_success() {
            return Err(failure(
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    "not_found"
                } else {
                    "api"
                },
                "GCP read request returned an unsuccessful status",
            ));
        }
        let bytes = timeout_at(deadline, response.bytes())
            .await
            .map_err(|_| failure("timeout", "GCP response read timed out"))?
            .map_err(|_| failure("transport", "GCP response read failed"))?;
        if bytes.len() > MAX_API_RESPONSE_BYTES {
            return Err(failure(
                "response",
                "GCP response exceeded the configured size limit",
            ));
        }
        let parsed = parse_json_under_deadline(bytes.to_vec(), deadline).await?;
        Ok((parsed, request_id))
    }

    pub(super) async fn list_all<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        origin: &str,
        base_path: &str,
        deadline: Instant,
    ) -> Result<(Vec<T>, Vec<String>), CloudError> {
        let mut rows = Vec::new();
        let mut request_ids = Vec::new();
        let mut page_token: Option<String> = None;
        let mut seen_page_tokens = BTreeSet::new();
        loop {
            let mut path = base_path.to_owned();
            if let Some(token) = &page_token {
                path.push_str(if path.contains('?') { "&" } else { "?" });
                path.push_str("pageToken=");
                path.push_str(&urlencoding::encode(token));
            }
            let (page, request_id): (ComputeList<T>, Option<String>) =
                self.request_json(origin, &path, deadline).await?;
            rows.extend(page.items);
            if let Some(id) = request_id {
                request_ids.push(id);
            }
            let Some(next_token) = page.next_page_token.filter(|token| !token.is_empty()) else {
                break;
            };
            if !seen_page_tokens.insert(next_token.clone()) || seen_page_tokens.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "GCP Compute pagination did not make bounded progress",
                ));
            }
            page_token = Some(next_token);
        }
        Ok((rows, request_ids))
    }
}

pub(super) fn image_from_binding(binding: &GcpImageBinding) -> GcpImage {
    GcpImage {
        image_id: binding.image_id.clone(),
        os: binding.os.clone(),
        os_source: binding.os_source.clone(),
        architecture: binding.architecture.clone(),
        capabilities: binding.capabilities.clone(),
        capabilities_source: binding.capabilities_source.clone(),
        license_free_source: binding.license_free_source.clone(),
        license_facts: binding.license_facts.clone(),
    }
}

pub(super) fn path_segment(value: &str) -> Result<String, CloudError> {
    if value.is_empty() || value.contains('/') || value == "." || value == ".." {
        return Err(failure(
            "configuration",
            "GCP resource path segment is invalid",
        ));
    }
    Ok(urlencoding::encode(value).into_owned())
}

pub(super) fn resource_url(origin: &str, path: &str) -> Result<Url, CloudError> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("\\")
        || path.split('/').any(|part| part == ".." || part == ".")
    {
        return Err(failure("configuration", "GCP resource path is unsafe"));
    }
    let url =
        Url::parse(origin).map_err(|_| failure("configuration", "GCP API origin is invalid"))?;
    url.join(path)
        .map_err(|_| failure("configuration", "GCP resource URL is invalid"))
}

pub(super) fn validate_origin(origin: &str) -> Result<(), CloudError> {
    let url =
        Url::parse(origin).map_err(|_| failure("configuration", "GCP API origin is invalid"))?;
    let test_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    let host = url.host_str();
    let production = match url.scheme() {
        "https" => matches!(
            host,
            Some("compute.googleapis.com" | "cloudbilling.googleapis.com")
        ),
        "http" => test_http,
        _ => false,
    };
    if !production
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(failure("configuration", "GCP API origin is unsafe"));
    }
    Ok(())
}

pub(super) async fn parse_json_under_deadline<T: for<'de> Deserialize<'de> + Send + 'static>(
    bytes: Vec<u8>,
    deadline: Instant,
) -> Result<T, CloudError> {
    timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes)),
    )
    .await
    .map_err(|_| failure("timeout", "GCP response parsing timed out"))?
    .map_err(|_| failure("response", "GCP response parser task failed"))?
    .map_err(|error| {
        failure(
            "response",
            &format!("GCP response JSON was invalid: {error}"),
        )
    })
}

async fn read_child_configuration(
    path: &std::path::Path,
    deadline: Instant,
) -> Result<(GcpChildConfiguration, Vec<u8>), CloudError> {
    let bytes = timeout_at(deadline, tokio::fs::read(path))
        .await
        .map_err(|_| failure("timeout", "GCP child configuration read timed out"))?
        .map_err(|_| failure("configuration", "GCP child configuration could not be read"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| failure("configuration", "GCP child configuration is not UTF-8"))?;
    let child = toml_edit::de::from_str(text)
        .map_err(|_| failure("configuration", "GCP child configuration is invalid TOML"))?;
    Ok((child, bytes))
}

pub(super) fn validate_child_binding(
    settings: &GcpSettings,
    child: &GcpChildConfiguration,
    bytes: &[u8],
) -> Result<(), CloudError> {
    let actual_hash = hex::encode(Sha256::digest(bytes));
    if actual_hash != settings.configuration_sha256
        || child.project_id != settings.project_id
        || child.zone != settings.zone
        || child.credentials_file.is_empty()
        || child.network_id.is_empty()
        || child.subnetwork_id.is_empty()
    {
        return Err(failure(
            "configuration",
            "GCP child config hash, project, zone, network, or credentials do not match the binding",
        ));
    }
    Ok(())
}
