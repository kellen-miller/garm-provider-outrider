use super::{
    AwsBackend, AwsCatalogBinding, AwsChildConfiguration, AwsImage, AwsImageBinding, AwsLocation,
    AwsSettings, BehaviorVersion, CloudError, Credentials, Digest, Ec2Client, Instant,
    PricingClient, ProviderBinding, Region, Sha256, StsClient, failure, is_sha256, sdk_failure,
    timeout_at,
};

impl AwsBackend {
    /// Build AWS catalog settings from central catalog and child-provider bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when the central catalog binding does not point to the
    /// pinned AWS child configuration.
    pub fn settings_from_bindings(
        catalog: &AwsCatalogBinding,
        provider: &ProviderBinding,
        include_spot_placement_scores: bool,
    ) -> Result<AwsSettings, CloudError> {
        if provider.id != catalog.provider_binding_id
            || provider.cloud != "aws"
            || provider.account != catalog.account_id
            || provider.location != catalog.region
            || catalog.credential_reference
                != format!("provider-binding:{}:configuration", provider.id)
            || catalog.id.is_empty()
            || catalog.locations.is_empty()
            || catalog.images.is_empty()
            || catalog.storage_classes.is_empty()
        {
            return Err(failure(
                "configuration",
                "AWS catalog binding does not match its pinned child-provider binding",
                vec![],
            ));
        }

        let locations = catalog
            .locations
            .iter()
            .map(|(name, network)| {
                (
                    name.clone(),
                    AwsLocation {
                        region: catalog.region.clone(),
                        availability_zone: network.availability_zone.clone(),
                        subnet_id: network.subnet_id.clone(),
                        security_group_ids: network.security_group_ids.clone(),
                    },
                )
            })
            .collect();
        let images = catalog
            .images
            .iter()
            .map(|(name, image)| (name.clone(), aws_image_from_binding(image)))
            .collect();
        Ok(AwsSettings {
            account_id: catalog.account_id.clone(),
            region: catalog.region.clone(),
            pricing_location: catalog.pricing_location.clone(),
            locations,
            images,
            storage_classes: catalog.storage_classes.clone(),
            include_spot_placement_scores,
            ec2_endpoint_url: None,
            pricing_endpoint_url: None,
            sts_endpoint_url: None,
            catalog_id: catalog.id.clone(),
            binding_id: provider.id.clone(),
            source_revision: provider.source_revision.clone(),
            binary_sha256: provider.executable_sha256.clone(),
            configuration_sha256: provider.configuration_sha256.clone(),
            provider_configuration_path: provider.configuration_path.clone(),
        })
    }

    /// Load AWS credentials and initialize clients for the configured region.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration validation or AWS identity
    /// verification fails.
    pub async fn connect(settings: AwsSettings, deadline: Instant) -> Result<Self, CloudError> {
        let child = read_child_configuration(
            &settings.provider_configuration_path,
            &settings.configuration_sha256,
            deadline,
        )
        .await?;
        validate_child_binding(&settings, &child)?;
        if child.credentials.credential_type != "static" {
            return Err(failure(
                "unsupported",
                "AWS role credentials cannot be verified against the pinned child configuration",
                vec![],
            ));
        }

        let credentials = child.credentials.static_credentials.ok_or_else(|| {
            failure(
                "configuration",
                "AWS static credentials are missing",
                vec![],
            )
        })?;
        if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
            return Err(failure(
                "configuration",
                "AWS static credentials are incomplete",
                vec![],
            ));
        }

        let shared = timeout_at(
            deadline,
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(settings.region.clone()))
                .credentials_provider(Credentials::new(
                    credentials.access_key_id,
                    credentials.secret_access_key,
                    credentials.session_token,
                    None,
                    "outrider-pinned-child-config",
                ))
                .load(),
        )
        .await
        .map_err(|_| failure("timeout", "AWS credential loading timed out", vec![]))?;
        let mut ec2_config = aws_sdk_ec2::config::Builder::from(&shared);
        if let Some(endpoint) = &settings.ec2_endpoint_url {
            ec2_config = ec2_config.endpoint_url(endpoint);
        }

        let ec2 = Ec2Client::from_conf(ec2_config.build());
        let mut pricing_config =
            aws_sdk_pricing::config::Builder::from(&shared).region(Region::new("us-east-1"));
        if let Some(endpoint) = &settings.pricing_endpoint_url {
            pricing_config = pricing_config.endpoint_url(endpoint);
        }

        let pricing = PricingClient::from_conf(pricing_config.build());
        let mut sts_config = aws_sdk_sts::config::Builder::from(&shared);
        if let Some(endpoint) = &settings.sts_endpoint_url {
            sts_config = sts_config.endpoint_url(endpoint);
        }

        let sts = StsClient::from_conf(sts_config.build());
        Self::with_clients(settings, ec2, pricing, sts, deadline).await
    }

    /// Validate settings and clients against the configured AWS account.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid placement settings or when AWS caller
    /// identity does not match the configured account.
    pub async fn with_clients(
        settings: AwsSettings,
        ec2: Ec2Client,
        pricing: PricingClient,
        sts: StsClient,
        deadline: Instant,
    ) -> Result<Self, CloudError> {
        let child = read_child_configuration(
            &settings.provider_configuration_path,
            &settings.configuration_sha256,
            deadline,
        )
        .await?;
        validate_child_binding(&settings, &child)?;
        if settings.account_id.is_empty() || settings.region.is_empty() {
            return Err(failure(
                "configuration",
                "AWS account and region are required",
                vec![],
            ));
        }

        if settings.binding_id.is_empty()
            || settings.catalog_id.is_empty()
            || settings.source_revision.is_empty()
            || !is_sha256(&settings.binary_sha256)
            || !is_sha256(&settings.configuration_sha256)
        {
            return Err(failure(
                "configuration",
                "AWS provider pin metadata is invalid",
                vec![],
            ));
        }

        if settings
            .locations
            .values()
            .any(|location| location.region != settings.region)
        {
            return Err(failure(
                "configuration",
                "each AWS location must use the configured client region",
                vec![],
            ));
        }

        if settings.storage_classes.is_empty()
            || settings
                .storage_classes
                .values()
                .any(|volume_type| volume_type != "gp3")
        {
            return Err(failure(
                "configuration",
                "AWS storage classes must map to a supported physical volume type",
                vec![],
            ));
        }

        if settings
            .locations
            .values()
            .any(|location| location.security_group_ids.is_empty())
        {
            return Err(failure(
                "configuration",
                "each AWS location requires an explicit security group",
                vec![],
            ));
        }

        let identity = timeout_at(deadline, sts.get_caller_identity().send())
            .await
            .map_err(|_| failure("timeout", "AWS identity verification timed out", vec![]))?
            .map_err(|error| {
                sdk_failure(&error, "AWS caller identity verification failed", vec![])
            })?;
        if identity.account() != Some(settings.account_id.as_str()) {
            return Err(failure(
                "ownership",
                "AWS credentials belong to a different account than configured",
                vec![],
            ));
        }

        Ok(Self {
            settings,
            ec2,
            pricing,
        })
    }
}

async fn read_child_configuration(
    path: &std::path::Path,
    expected_sha256: &str,
    deadline: Instant,
) -> Result<AwsChildConfiguration, CloudError> {
    let bytes = timeout_at(deadline, tokio::fs::read(path))
        .await
        .map_err(|_| failure("timeout", "AWS child configuration read timed out", vec![]))?
        .map_err(|_| {
            failure(
                "configuration",
                "AWS child configuration could not be read",
                vec![],
            )
        })?;
    let actual_sha256 = hex::encode(Sha256::digest(&bytes));
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(failure(
            "configuration",
            "AWS child configuration does not match its pinned fingerprint",
            vec![],
        ));
    }

    let contents = std::str::from_utf8(&bytes).map_err(|_| {
        failure(
            "configuration",
            "AWS child configuration is not UTF-8",
            vec![],
        )
    })?;
    toml_edit::de::from_str(contents).map_err(|_| {
        failure(
            "configuration",
            "AWS child configuration is invalid",
            vec![],
        )
    })
}

pub(super) fn aws_image_from_binding(image: &AwsImageBinding) -> AwsImage {
    AwsImage {
        image_id: image.image_id.clone(),
        os: image.os.clone(),
        architecture: image.architecture.clone(),
        capabilities: image.capabilities.clone(),
        windows_license_included: image.windows_license_included,
        cloud_init: image.cloud_init,
        ec2_launch_agent: image.ec2_launch_agent,
    }
}

pub(super) fn validate_child_binding(
    settings: &AwsSettings,
    child: &AwsChildConfiguration,
) -> Result<(), CloudError> {
    if child.region != settings.region || child.subnet_id.is_empty() {
        return Err(failure(
            "configuration",
            "AWS child region or subnet is invalid for the central catalog binding",
            vec![],
        ));
    }

    if child.credentials.credential_type == "static"
        && child.credentials.static_credentials.is_none()
    {
        return Err(failure(
            "configuration",
            "AWS static credentials are missing",
            vec![],
        ));
    }

    if child.credentials.credential_type != "static" && child.credentials.credential_type != "role"
    {
        return Err(failure(
            "configuration",
            "AWS credential type is unsupported",
            vec![],
        ));
    }

    Ok(())
}
