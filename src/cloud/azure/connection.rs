use super::{
    Arc, AzureBackend, AzureCatalogBinding, AzureChildConfiguration, AzureImage, AzureImageBinding,
    AzureLocation, AzureSettings, BTreeMap, ClientSecretCredential, CloudError, Decimal, Digest,
    Instant, MANAGEMENT_ORIGIN, MAX_API_RESPONSE_BYTES, Mutex, PathBuf, ProviderBinding,
    RetailPage, Secret, Sha256, TokenCredential, failure, is_sha256, timeout_at,
};

impl AzureBackend {
    /// Build Azure catalog settings from central and executable-provider bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when the bindings disagree or required catalog data is missing.
    pub fn settings_from_bindings(
        catalog: &AzureCatalogBinding,
        provider: &ProviderBinding,
    ) -> Result<AzureSettings, CloudError> {
        if provider.id != catalog.provider_binding_id
            || provider.cloud != "azure"
            || provider.account != catalog.account_id
            || provider.location != catalog.pricing_location
            || catalog.credential_reference
                != format!("provider-binding:{}:configuration", provider.id)
            || catalog.id.is_empty()
            || catalog.subscription_id.is_empty()
            || catalog.locations.is_empty()
            || catalog.images.is_empty()
            || catalog.storage_classes.is_empty()
            || catalog.accelerator_facts.iter().any(|(sku, facts)| {
                sku.is_empty()
                    || facts.vendor.is_empty()
                    || facts.model.is_empty()
                    || facts.total_memory_gib <= Decimal::ZERO
                    || facts.source.is_empty()
            })
        {
            return Err(failure(
                "configuration",
                "Azure catalog binding does not match its pinned child-provider binding",
            ));
        }

        Ok(AzureSettings {
            catalog_id: catalog.id.clone(),
            account_id: catalog.account_id.clone(),
            subscription_id: catalog.subscription_id.clone(),
            pricing_location: catalog.pricing_location.clone(),
            locations: catalog
                .locations
                .iter()
                .map(|(name, location)| {
                    (
                        name.clone(),
                        AzureLocation {
                            location: location.location.clone(),
                            vnet_subnet_id: location.vnet_subnet_id.clone(),
                            public_ip_sku: location.public_ip_sku.clone(),
                        },
                    )
                })
                .collect(),
            images: catalog
                .images
                .iter()
                .map(|(name, image)| (name.clone(), azure_image_from_binding(image)))
                .collect(),
            storage_classes: catalog.storage_classes.clone(),
            accelerator_facts: catalog.accelerator_facts.clone(),
            binding_id: provider.id.clone(),
            source_revision: provider.source_revision.clone(),
            binary_sha256: provider.executable_sha256.clone(),
            configuration_sha256: provider.configuration_sha256.clone(),
            provider_configuration_path: provider.configuration_path.clone(),
            management_endpoint: MANAGEMENT_ORIGIN.to_owned(),
            retail_endpoint: "https://prices.azure.com".to_owned(),
        })
    }

    /// Connect with the service principal verified from the pinned child configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, identity, authentication, or client setup fails.
    pub async fn connect(settings: AzureSettings, deadline: Instant) -> Result<Self, CloudError> {
        let (child, bytes) =
            read_child_configuration(&settings.provider_configuration_path, deadline).await?;
        validate_child_binding(&settings, &child, &bytes)?;
        if child.credentials.workload_identity.is_some()
            || child.credentials.managed_identity.is_some()
        {
            return Err(failure(
                "unsupported",
                "Azure workload or managed identity is not verified against the pinned child configuration",
            ));
        }
        let principal = child
            .credentials
            .service_principal
            .as_ref()
            .ok_or_else(|| {
                failure(
                    "unsupported",
                    "Azure service principal credentials are required",
                )
            })?;
        if principal.tenant_id.is_empty()
            || principal.client_id.is_empty()
            || principal.client_secret.is_empty()
        {
            return Err(failure(
                "configuration",
                "Azure service principal credentials are incomplete",
            ));
        }
        if !child.credentials.client_options.is_empty() {
            return Err(failure(
                "unsupported",
                "Azure custom identity client options are not verified against the pinned provider",
            ));
        }
        let credential = ClientSecretCredential::new(
            &principal.tenant_id,
            principal.client_id.clone(),
            Secret::new(principal.client_secret.clone()),
            None,
        )
        .map_err(|_| {
            failure(
                "configuration",
                "Azure service principal credentials are invalid",
            )
        })?;
        let credential: Arc<dyn TokenCredential> = credential;
        timeout_at(
            deadline,
            credential.get_token(&["https://management.azure.com/.default"], None),
        )
        .await
        .map_err(|_| failure("timeout", "Azure credential loading timed out"))?
        .map_err(|_| {
            failure(
                "authentication",
                "Azure service principal authentication failed",
            )
        })?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| failure("configuration", "Azure HTTP client construction failed"))?;
        Self::with_clients(settings, client, credential, deadline).await
    }

    /// Construct a catalog from explicit authenticated management and public HTTP clients.
    ///
    /// # Errors
    ///
    /// Returns an error when the child binding or supported Azure configuration is invalid.
    pub async fn with_clients(
        settings: AzureSettings,
        client: reqwest::Client,
        credential: Arc<dyn TokenCredential>,
        deadline: Instant,
    ) -> Result<Self, CloudError> {
        let (child, bytes) =
            read_child_configuration(&settings.provider_configuration_path, deadline).await?;
        validate_child_binding(&settings, &child, &bytes)?;
        if !child.credentials.client_options.is_empty() {
            return Err(failure(
                "unsupported",
                "Azure custom identity client options are not verified against the pinned provider",
            ));
        }
        if settings.locations.values().any(|location| {
            location.public_ip_sku.as_deref().is_some_and(|sku| {
                sku != "Standard"
                    || settings.source_revision != "10659879c5d91979c7f077439bd78e17622452a6"
            })
        }) {
            return Err(failure(
                "unsupported",
                "Azure public IP allocation requires exact SKU Standard and qualified child revision 10659879c5d91979c7f077439bd78e17622452a6 (static IPv4, Regional tier)",
            ));
        }
        if child.use_ephemeral_storage {
            return Err(failure(
                "unsupported",
                "qualified Azure child ephemeral OS disks are not priced as managed disks",
            ));
        }
        if settings.account_id.is_empty()
            || settings.subscription_id.is_empty()
            || settings.pricing_location.is_empty()
            || settings.binding_id.is_empty()
            || settings.catalog_id.is_empty()
            || settings.source_revision.is_empty()
            || !is_sha256(&settings.binary_sha256)
            || !is_sha256(&settings.configuration_sha256)
            || settings.locations.values().any(|location| {
                location.location != child.location
                    || (child.disable_isolated_networks
                        && location.vnet_subnet_id.as_deref()
                            != Some(child.vnet_subnet_id.as_str()))
                    || (!child.disable_isolated_networks && location.vnet_subnet_id.is_some())
            })
            || settings.accelerator_facts.iter().any(|(sku, facts)| {
                sku.is_empty()
                    || facts.vendor.trim().is_empty()
                    || facts.model.trim().is_empty()
                    || facts.total_memory_gib <= Decimal::ZERO
                    || facts.source.trim().is_empty()
            })
            || settings.images.values().any(|image| {
                image.licensing_source.trim().is_empty()
                    || image.minimum_os_disk_source.trim().is_empty()
                    || image.minimum_os_disk_gib == 0
            })
            || settings.storage_classes.is_empty()
            || settings.storage_classes.values().any(|sku| {
                !matches!(
                    sku.as_str(),
                    "Standard_LRS" | "StandardSSD_LRS" | "Premium_LRS"
                )
            })
        {
            return Err(failure(
                "configuration",
                "Azure catalog settings do not match the verified child binding",
            ));
        }
        let endpoint = reqwest::Url::parse(&settings.management_endpoint)
            .map_err(|_| failure("configuration", "Azure management endpoint is invalid"))?;
        if endpoint.scheme() != "https"
            && !endpoint
                .host_str()
                .is_some_and(|host| host == "127.0.0.1" || host == "localhost")
        {
            return Err(failure(
                "configuration",
                "Azure management endpoint must use HTTPS",
            ));
        }
        if Instant::now() >= deadline {
            return Err(failure("timeout", "Azure client initialization timed out"));
        }
        Ok(Self {
            settings,
            client,
            credential,
            child,
            retail_snapshots: Mutex::new(BTreeMap::new()),
        })
    }
    pub(super) async fn get_management(
        &self,
        url: &str,
        expected_prefix: &str,
        deadline: Instant,
    ) -> Result<serde_json::Value, CloudError> {
        let response = self.send_management(url, expected_prefix, deadline).await?;
        if !response.status().is_success() {
            return Err(failure(
                "upstream",
                "Azure management request returned an error",
            ));
        }
        management_json(response, deadline).await
    }

    pub(super) async fn get_management_optional(
        &self,
        url: &str,
        expected_path: &str,
        deadline: Instant,
    ) -> Result<Option<serde_json::Value>, CloudError> {
        let response = self.send_management(url, expected_path, deadline).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(failure(
                "upstream",
                "Azure management request returned an error",
            ));
        }
        let value = management_json(response, deadline).await?;
        Ok(Some(value))
    }

    pub(super) async fn post_management_json(
        &self,
        url: &str,
        expected_path: &str,
        body: &serde_json::Value,
        deadline: Instant,
    ) -> Result<serde_json::Value, CloudError> {
        let target = reqwest::Url::parse(url)
            .map_err(|_| failure("pagination", "Azure Resource Graph URL is invalid"))?;
        let origin = reqwest::Url::parse(&self.settings.management_endpoint)
            .map_err(|_| failure("configuration", "Azure management endpoint is invalid"))?;
        if target.origin() != origin.origin() || target.path() != expected_path {
            return Err(failure(
                "pagination",
                "Azure Resource Graph URL is outside the retained management endpoint",
            ));
        }
        let token = timeout_at(
            deadline,
            self.credential
                .get_token(&["https://management.azure.com/.default"], None),
        )
        .await
        .map_err(|_| failure("timeout", "Azure token refresh timed out"))?
        .map_err(|_| {
            failure(
                "authentication",
                "Azure service principal authentication failed",
            )
        })?;
        let response = timeout_at(
            deadline,
            self.client
                .post(target)
                .bearer_auth(token.token.secret())
                .json(body)
                .send(),
        )
        .await
        .map_err(|_| failure("timeout", "Azure Resource Graph request timed out"))?
        .map_err(|_| failure("network", "Azure Resource Graph request failed"))?;
        if !response.status().is_success() {
            return Err(failure(
                "upstream",
                "Azure Resource Graph request returned an error",
            ));
        }
        management_json(response, deadline).await
    }

    pub(super) async fn send_management(
        &self,
        url: &str,
        expected_path: &str,
        deadline: Instant,
    ) -> Result<reqwest::Response, CloudError> {
        let target = reqwest::Url::parse(url)
            .map_err(|_| failure("pagination", "Azure management URL is invalid"))?;
        let origin = reqwest::Url::parse(&self.settings.management_endpoint)
            .map_err(|_| failure("configuration", "Azure management endpoint is invalid"))?;
        if target.origin() != origin.origin() || target.path() != expected_path {
            return Err(failure(
                "pagination",
                "Azure management pagination target is outside the expected origin or resource path",
            ));
        }
        let token = timeout_at(
            deadline,
            self.credential
                .get_token(&["https://management.azure.com/.default"], None),
        )
        .await
        .map_err(|_| failure("timeout", "Azure token refresh timed out"))?
        .map_err(|_| {
            failure(
                "authentication",
                "Azure service principal authentication failed",
            )
        })?;
        let response = timeout_at(
            deadline,
            self.client
                .get(target)
                .bearer_auth(token.token.secret())
                .send(),
        )
        .await
        .map_err(|_| failure("timeout", "Azure management request timed out"))?
        .map_err(|_| failure("network", "Azure management request failed"))?;
        Ok(response)
    }

    pub(super) async fn get_retail(
        &self,
        url: &str,
        deadline: Instant,
    ) -> Result<RetailPage, CloudError> {
        let target = reqwest::Url::parse(url)
            .map_err(|_| failure("pagination", "Azure Retail URL is invalid"))?;
        let origin = reqwest::Url::parse(&self.settings.retail_endpoint)
            .map_err(|_| failure("configuration", "Azure Retail endpoint is invalid"))?;
        if target.origin() != origin.origin() || target.path() != "/api/retail/prices" {
            return Err(failure(
                "pagination",
                "Azure Retail pagination target is outside the public Retail API",
            ));
        }
        let mut throttle_retries = 0;
        let response = loop {
            let response = timeout_at(deadline, self.client.get(target.clone()).send())
                .await
                .map_err(|_| failure("timeout", "Azure Retail request timed out"))?
                .map_err(|_| failure("network", "Azure Retail request failed"))?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                break response
                    .error_for_status()
                    .map_err(|_| failure("upstream", "Azure Retail request returned an error"))?;
            }

            if throttle_retries >= 3 {
                return Err(failure(
                    "throttled",
                    "Azure Retail throttle retries exhausted",
                ));
            }

            let delay_seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    failure(
                        "throttled",
                        "Azure Retail Retry-After seconds are unavailable",
                    )
                })?;
            let retry_at = Instant::now()
                .checked_add(std::time::Duration::from_secs(delay_seconds.max(1)))
                .filter(|retry_at| *retry_at < deadline)
                .ok_or_else(|| {
                    failure("timeout", "Azure Retail retry exceeds the request deadline")
                })?;
            drop(response);
            tokio::time::sleep_until(retry_at).await;
            throttle_retries += 1;
        };

        timeout_at(deadline, response.json())
            .await
            .map_err(|_| failure("timeout", "Azure Retail response timed out"))?
            .map_err(|_| failure("protocol", "Azure Retail response was invalid"))
    }
}

pub(super) fn azure_image_from_binding(image: &AzureImageBinding) -> AzureImage {
    AzureImage {
        image_id: image.image_id.clone(),
        minimum_os_disk_gib: image.minimum_os_disk_gib,
        minimum_os_disk_source: image.minimum_os_disk_source.clone(),
        capabilities: image.capabilities.clone(),
        licensing: image.licensing,
        licensing_source: image.licensing_source.clone(),
    }
}

async fn read_child_configuration(
    path: &PathBuf,
    deadline: Instant,
) -> Result<(AzureChildConfiguration, Vec<u8>), CloudError> {
    let bytes = timeout_at(deadline, tokio::fs::read(path))
        .await
        .map_err(|_| failure("timeout", "Azure child configuration read timed out"))?
        .map_err(|_| {
            failure(
                "configuration",
                "Azure child configuration could not be read",
            )
        })?;
    let parsed = toml_edit::de::from_slice::<AzureChildConfiguration>(&bytes)
        .map_err(|_| failure("configuration", "Azure child configuration is invalid"))?;
    Ok((parsed, bytes))
}

async fn management_json(
    mut response: reqwest::Response,
    deadline: Instant,
) -> Result<serde_json::Value, CloudError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = timeout_at(deadline, response.chunk())
        .await
        .map_err(|_| failure("timeout", "Azure management response timed out"))?
        .map_err(|_| failure("protocol", "Azure management response was invalid"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_API_RESPONSE_BYTES {
            return Err(failure(
                "response",
                "Azure management response exceeded the configured size limit",
            ));
        }

        bytes.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&bytes)
        .map_err(|_| failure("protocol", "Azure management response was invalid"))
}

pub(super) fn validate_child_binding(
    settings: &AzureSettings,
    child: &AzureChildConfiguration,
    bytes: &[u8],
) -> Result<(), CloudError> {
    let digest = hex::encode(Sha256::digest(bytes));
    if digest != settings.configuration_sha256
        || child.credentials.subscription_id != settings.subscription_id
        || child.location != settings.pricing_location
        || (child.disable_isolated_networks && child.vnet_subnet_id.is_empty())
    {
        return Err(failure(
            "configuration",
            "Azure pinned child configuration subscription, location, network, or digest does not match catalog binding",
        ));
    }
    Ok(())
}
