use serde_json::{Value, json};

/// Describe the authored administrator-owned `config.toml` data model.
/// Repository documents instead accept only `version` and `profiles`.
pub fn json_schema() -> Value {
    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "urn:outrider:config:v1",
        "type": "object",
        "title": "Outrider installation configuration",
        "required": ["version"],
        "additionalProperties": false,
        "properties": {
            "version": {"const": 1},
            "profiles": {"type":"object", "additionalProperties":{"$ref":"#/$defs/RunnerProfilePatch"}},
            "defaults": {"$ref":"#/$defs/RunnerProfilePatch"},
            "limits": {"$ref":"#/$defs/AdministratorLimits"},
            "default_purchasing": {"type":"string"},
            "default_policy": {"type":"string"},
            "policies": {"type":"object", "additionalProperties":{"$ref":"#/$defs/PolicyDefinition"}},
            "cost_horizon_seconds": {"type":"integer", "minimum":0},
            "max_price_age_seconds": {"type":"integer", "minimum":0},
            "history": {"$ref":"#/$defs/HistorySettings"},
            "control": {"$ref":"#/$defs/ControlSettings"},
            "admission": {"$ref":"#/$defs/AdmissionSettings"},
            "ingress": {"$ref":"#/$defs/IngressSettings"},
        },
        "$defs": {
            "RunnerProfilePatch": {
                "type":"object", "additionalProperties":false,
                "properties": {
                    "os":{"type":"string"}, "architecture":{"type":"string"}, "image":{"type":"string"},
                    "vcpu_min":{"type":"integer","minimum":0}, "vcpu_max":{"type":"integer","minimum":0},
                    "memory_gib_min":{"type":"integer","minimum":0}, "memory_gib_max":{"type":"integer","minimum":0},
                    "machine_types":{"$ref":"#/$defs/StringListMap"}, "machine_families":{"$ref":"#/$defs/StringListMap"},
                    "accounts":{"$ref":"#/$defs/StringList"}, "locations":{"$ref":"#/$defs/StringList"},
                    "gpu":{"$ref":"#/$defs/GpuRequirementPatch"}, "boot_disk":{"$ref":"#/$defs/DiskRequirementPatch"},
                    "local_scratch_gib":{"type":"integer","minimum":0}, "capabilities":{"$ref":"#/$defs/StringSet"},
                    "purchasing":{"type":"string"}, "purchasing_models":{"$ref":"#/$defs/StringSet"},
                    "purchasing_tiers":{"type":"array","items":{"$ref":"#/$defs/StringList"}},
                    "maximum_total_cost":{"type":["number","string"]}, "policy":{"type":"string"}
                }
            },
            "AdministratorLimits": {
                "type":"object", "additionalProperties":false,
                "required":["max_vcpu","max_memory_gib","max_boot_disk_gib","max_boot_disk_iops","max_boot_disk_throughput_mib_s","max_local_scratch_gib","max_gpu_count","max_gpu_memory_gib","max_allocations_controller","max_allocations_account","max_allocations_pool","allowed_clouds","allowed_accounts","allowed_locations","allowed_machine_types","allowed_machine_families","allowed_gpu_vendors","allowed_gpu_models","allowed_os","allowed_architectures","allowed_images","allowed_capabilities","allowed_purchasing"],
                "properties": {
                    "max_vcpu":{"type":"integer","minimum":0}, "max_memory_gib":{"type":"integer","minimum":0},
                    "max_boot_disk_gib":{"type":"integer","minimum":0}, "max_boot_disk_iops":{"type":"integer","minimum":0},
                    "max_boot_disk_throughput_mib_s":{"type":"integer","minimum":0}, "max_local_scratch_gib":{"type":"integer","minimum":0},
                    "max_gpu_count":{"type":"integer","minimum":0}, "max_gpu_memory_gib":{"type":"integer","minimum":0},
                    "max_allocations_controller":{"type":"integer","minimum":1}, "max_allocations_account":{"type":"integer","minimum":1}, "max_allocations_pool":{"type":"integer","minimum":1},
                    "max_total_cost":{"type":["number","string"]}, "allowed_clouds":{"$ref":"#/$defs/StringSet"},
                    "allowed_accounts":{"$ref":"#/$defs/StringSet"}, "allowed_locations":{"$ref":"#/$defs/StringSet"},
                    "allowed_machine_types":{"$ref":"#/$defs/StringSetMap"}, "allowed_machine_families":{"$ref":"#/$defs/StringSetMap"},
                    "allowed_gpu_vendors":{"$ref":"#/$defs/StringSet"}, "allowed_gpu_models":{"$ref":"#/$defs/StringSet"},
                    "allowed_os":{"$ref":"#/$defs/StringSet"}, "allowed_architectures":{"$ref":"#/$defs/StringSet"},
                    "allowed_images":{"$ref":"#/$defs/StringSet"}, "allowed_capabilities":{"$ref":"#/$defs/StringSet"},
                    "allowed_purchasing":{"$ref":"#/$defs/StringSet"}
                }
            },
            "GpuRequirementPatch": {
                "type":"object", "additionalProperties":false,
                "properties":{"vendor":{"type":"string"},"model":{"type":"string"},"count":{"type":"integer","minimum":0},"memory_gib":{"type":"integer","minimum":0}}
            },
            "DiskRequirementPatch": {
                "type":"object", "additionalProperties":false,
                "properties":{"class":{"type":"string"},"size_gib":{"type":"integer","minimum":0},"iops":{"type":"integer","minimum":0},"throughput_mib_s":{"type":"integer","minimum":0}}
            },
            "ControlSettings": {
                "type":"object", "additionalProperties":false,
                "required":["listen_address","database_path","auth_token_file","provider_timeout_seconds"],
                "properties":{"listen_address":{"type":"string","minLength":1},"database_path":{"type":"string","minLength":1},"auth_token_file":{"type":"string","minLength":1},"provider_timeout_seconds":{"type":"integer","minimum":1}}
            },
            "IngressSettings": {
                "type":"object", "additionalProperties":false,
                "required":["github_api_origin","github_token_file","garm_api_origin","garm_token_file","controller_id","provider_name","pool_flavor","pool_image","runner_prefix","max_dynamic_pools","max_runners_per_pool","allocation_timeout_seconds","bootstrap_timeout_minutes","timeout_margin_seconds","pool_ttl_seconds","webhook_max_bytes","repositories"],
                "properties":{
                    "github_api_origin":{"type":"string","format":"uri"}, "github_token_file":{"type":"string","minLength":1},
                    "garm_api_origin":{"type":"string","format":"uri"}, "garm_token_file":{"type":"string","minLength":1}, "controller_id":{"type":"string","format":"uuid"},
                    "provider_name":{"type":"string","minLength":1}, "pool_flavor":{"type":"string","minLength":1}, "pool_image":{"type":"string","minLength":1}, "runner_prefix":{"type":"string","minLength":1},
                    "max_dynamic_pools":{"type":"integer","minimum":1}, "max_runners_per_pool":{"type":"integer","minimum":1},
                    "allocation_timeout_seconds":{"type":"integer","minimum":1}, "bootstrap_timeout_minutes":{"type":"integer","minimum":1},
                    "timeout_margin_seconds":{"type":"integer","minimum":1}, "pool_ttl_seconds":{"type":"integer","minimum":1},
                    "webhook_max_bytes":{"type":"integer","minimum":1},
                    "repositories":{"type":"object","additionalProperties":{"$ref":"#/$defs/IngressRepository"}}
                }
            },
            "IngressRepository": {
                "type":"object", "additionalProperties":false,
                "required":["garm_repository_id","webhook_secret_file"],
                "properties":{"garm_repository_id":{"type":"string","format":"uuid"},"webhook_secret_file":{"type":"string","minLength":1},"allow_forks":{"type":"boolean","default":false},"template_id":{"type":["integer","null"],"minimum":1}}
            },
            "PolicyDefinition": {"type":"object","additionalProperties":false,"required":["strategy","missing_data"],"properties":{"strategy":{"$ref":"#/$defs/Policy"},"missing_data":{"$ref":"#/$defs/MissingDataBehavior"}}},
            "Policy": {"oneOf":[
                {"type":"object","additionalProperties":false,"required":["strategy","max_price_age_seconds"],"properties":{"strategy":{"const":"economy"},"max_price_age_seconds":{"type":"integer","minimum":0}}},
                {"type":"object","additionalProperties":false,"required":["strategy","max_observation_age_seconds","min_samples"],"properties":{"strategy":{"const":"continuity"},"max_observation_age_seconds":{"type":"integer","minimum":0},"min_samples":{"type":"integer","minimum":0}}},
                {"type":"object","additionalProperties":false,"required":["strategy","cost_weight","failure_weight","interruption_weight","latency_weight","cost_scale","interruption_scale","latency_scale","min_samples","max_observation_age_seconds","max_price_age_seconds"],"properties":{"strategy":{"const":"weighted"},"cost_weight":{"type":["number","string"]},"failure_weight":{"type":["number","string"]},"interruption_weight":{"type":["number","string"]},"latency_weight":{"type":["number","string"]},"cost_scale":{"type":["number","string"]},"interruption_scale":{"type":["number","string"]},"latency_scale":{"type":["number","string"]},"min_samples":{"type":"integer","minimum":0},"max_observation_age_seconds":{"type":"integer","minimum":0},"max_price_age_seconds":{"type":"integer","minimum":0}}},
                {"type":"object","additionalProperties":false,"required":["strategy","identities"],"properties":{"strategy":{"const":"ordered"},"identities":{"$ref":"#/$defs/StringList"}}}
            ]},
            "MissingDataBehavior":{"oneOf":[{"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"const":"reject"}}},{"type":"object","additionalProperties":false,"required":["action","policy"],"properties":{"action":{"const":"fallback"},"policy":{"type":"string"}}}]},
            "StringList":{"type":"array","items":{"type":"string"}},
            "StringSet":{"type":"array","uniqueItems":true,"items":{"type":"string"}},
            "StringListMap":{"type":"object","additionalProperties":{"$ref":"#/$defs/StringList"}},
            "StringSetMap":{"type":"object","additionalProperties":{"$ref":"#/$defs/StringSet"}}

        }
    });
    schema["description"] =
        json!("Structured data model for administrator-owned config.toml documents.");
    schema["properties"]["garm"] = json!({"$ref":"#/$defs/GarmSource"});
    schema["properties"]["webhook_listen_address"] = json!({"type":"string","minLength":1});
    schema["properties"]["authentication"] = json!({
        "type":"object", "additionalProperties":false,
        "required":["github_app_id","github_installation_id","github_private_key_file","garm_credentials_file"],
        "properties":{
            "github_app_id":{"type":"integer","minimum":1},
            "github_installation_id":{"type":"integer","minimum":1},
            "github_private_key_file":{"type":"string","pattern":"^/"},
            "garm_credentials_file":{"type":"string","pattern":"^/"}
        }
    });
    schema["properties"]["providers"] =
        json!({"type":"array","items":{"$ref":"#/$defs/ProviderSource"}});
    schema["$defs"]["GarmSource"] = json!({
        "type":"object","additionalProperties":false,"required":["config_file"],
        "properties":{"config_file":{"type":"string","minLength":1}}
    });
    schema["$defs"]["ProviderSource"] = json!({
        "type":"object","additionalProperties":false,
        "required":["name","cloud"],
        "properties":{
            "name":{"type":"string","minLength":1},"cloud":{"enum":["aws","azure","gcp"]},
            "executable_sha256":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"},
            "configuration_sha256":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"},
            "source_revision":{"type":"string","pattern":"^[a-fA-F0-9]{40}$"},
            "expected_version":{"type":"string","minLength":1,"maxLength":128},
            "supported_os":{"$ref":"#/$defs/StringSet"},"supported_architectures":{"$ref":"#/$defs/StringSet"},
            "purchasing_capabilities":{"type":"array","minItems":1,"uniqueItems":true,"items":{"enum":["on_demand","spot"]}}
        }
    });
    schema["$defs"]["AdmissionSettings"] = json!({
        "type":"object", "additionalProperties":false,
        "required":["state_file","max_window_seconds"],
        "properties":{"state_file":{"type":"string","pattern":"^/"},"max_window_seconds":{"type":"integer","minimum":1}}
    });
    schema["properties"]["effective_prices"] = json!({
        "type":"array","items":{"$ref":"#/$defs/EffectivePrice"}
    });
    schema["$defs"]["EffectivePrice"] = effective_price_schema();
    schema["$defs"]["FinancialPriceScope"] = financial_price_scope_schema();
    schema["$defs"]["FinancialLaunchFacts"] = financial_launch_facts_schema();
    schema["$defs"]["HistorySettings"] = json!({
        "type":"object", "additionalProperties":false,
        "required":["window_seconds","sampling_interval_seconds","maximum_gap_seconds"],
        "properties":{"window_seconds":{"type":"integer","minimum":1},"sampling_interval_seconds":{"type":"integer","minimum":1},"maximum_gap_seconds":{"type":"integer","minimum":1}}
    });
    schema["properties"]["aws_catalog_bindings"] = json!({
        "type":"array",
        "items":{"$ref":"#/$defs/AwsCatalogSource"}
    });
    schema["properties"]["azure_catalog_bindings"] = json!({
        "type":"array",
        "items":{"$ref":"#/$defs/AzureCatalogSource"}
    });
    schema["properties"]["gcp_catalog_bindings"] = json!({
        "type":"array", "items":{"$ref":"#/$defs/GcpCatalogSource"}
    });
    schema["$defs"]["AwsCatalogSource"] = aws_catalog_binding_schema();
    schema["$defs"]["AwsNetworkSource"] = aws_network_binding_schema();
    schema["$defs"]["AwsImageBinding"] = aws_image_binding_schema();
    schema["$defs"]["AzureCatalogSource"] = azure_catalog_binding_schema();
    schema["$defs"]["AzureNetworkSource"] = azure_network_binding_schema();
    schema["$defs"]["AzureImageBinding"] = azure_image_binding_schema();
    schema["$defs"]["AzureAcceleratorFacts"] = azure_accelerator_facts_schema();
    schema["$defs"]["GcpCatalogSource"] = gcp_catalog_binding_schema();
    schema["$defs"]["GcpImageBinding"] = json!({
        "type":"object","additionalProperties":false,
        "required":["image_id","os","os_source","architecture","capabilities","capabilities_source","license_facts"],
        "properties":{"image_id":{"type":"string"},"os":{"type":"string"},"os_source":{"type":"string"},"architecture":{"type":"string"},"capabilities":{"$ref":"#/$defs/StringSet"},"capabilities_source":{"type":"string"},"license_free_source":{"type":["string","null"]},"license_facts":{"type":"object","additionalProperties":{"$ref":"#/$defs/GcpLicenseFacts"}}}
    });
    schema["$defs"]["GcpLicenseFacts"] = json!({
        "type":"object","additionalProperties":false,"required":["basis","source"],
        "properties":{"billing_sku":{"type":["string","null"]},"basis":{"enum":["included","per_vcpu","per_instance","sql_vcpu_floor"]},"source":{"type":"string","minLength":1}}
    });
    schema["$defs"]["GcpAcceleratorFacts"] = json!({
        "type":"object","additionalProperties":false,
        "required":["vendor","model","memory_gib","spot_billing_sku","source"],
        "properties":{"vendor":{"type":"string"},"model":{"type":"string"},"memory_gib":{"type":["number","string"]},"spot_billing_sku":{"type":"string","minLength":1},"source":{"type":"string"}}
    });
    schema["$defs"]["GcpMachineFacts"] = json!({
        "type":"object","additionalProperties":false,
        "required":["source"],
        "properties":{"architecture":{"type":["string","null"],"enum":["amd64","arm64",null]},"shared_cpu_billing_sku":{"type":["string","null"]},"bundled_local_ssd_gib":{"type":["integer","null"]},"bundled_local_ssd_partitions":{"type":["integer","null"]},"local_ssd_billing_sku":{"type":["string","null"]},"local_ssd_billing_quantity":{"type":["number","string","null"]},"max_pd_iops":{"type":["integer","null"]},"max_pd_throughput_mib_s":{"type":["number","string","null"]},"source":{"type":"string"}}
    });
    schema["$defs"]["GcpDiskPerformance"] = json!({
        "type":"object","additionalProperties":false,"required":["base_iops","iops_per_gib","maximum_iops","base_throughput_mib_s","throughput_per_gib_mib_s","maximum_throughput_mib_s","source"],
        "properties":{"base_iops":{"type":"integer"},"iops_per_gib":{"type":"integer"},"maximum_iops":{"type":"integer"},"base_throughput_mib_s":{"type":["number","string"]},"throughput_per_gib_mib_s":{"type":["number","string"]},"maximum_throughput_mib_s":{"type":["number","string"]},"source":{"type":"string","minLength":1}}
    });
    schema["properties"]["reruns"] = json!({"$ref":"#/$defs/RerunSettings"});
    schema["$defs"]["RerunSettings"] = json!({
        "type":"object", "additionalProperties":false,
        "required":["enabled","max_per_run","actions_write_permission"],
        "properties":{"enabled":{"type":"boolean"},"max_per_run":{"type":"integer","minimum":1,"maximum":100},"actions_write_permission":{"type":"boolean"}}
    });
    schema
}

fn effective_price_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,
        "required":["id","scope","component","currency","hourly_rate","source","reference","observed_at_unix_seconds","valid_from_unix_seconds","valid_until_unix_seconds"],
        "properties":{
            "id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9][A-Za-z0-9._:-]*$"},
            "scope":{"$ref":"#/$defs/FinancialPriceScope"},
            "component":{"enum":["compute","disk","licensing","public_address"]},
            "currency":{"const":"USD"},
            "hourly_rate":{"type":"string","pattern":effective_price_rate_pattern()},
            "source":{"type":"string","minLength":1,"maxLength":256,"pattern":"^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]+$"},
            "reference":{"type":"string","minLength":1,"maxLength":256,"pattern":"^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]+$"},
            "observed_at_unix_seconds":{"type":"integer","minimum":0},
            "valid_from_unix_seconds":{"type":"integer","minimum":0},
            "valid_until_unix_seconds":{"type":"integer","minimum":1}
        }
    })
}

fn effective_price_rate_pattern() -> String {
    let mut alternatives = vec!["0(\\.[0-9]{1,28})?".to_owned()];
    let maximum = "79228162514264337593543950335";
    for integer_digits in 1..=28 {
        let integer = if integer_digits == 1 {
            "[1-9]".to_owned()
        } else {
            format!("[1-9][0-9]{{{}}}", integer_digits - 1)
        };
        let small_rate_fraction_digits = 28 - integer_digits;
        if small_rate_fraction_digits == 0 {
            alternatives.push(integer);
        } else {
            alternatives.push(format!(
                "{integer}(\\.[0-9]{{1,{small_rate_fraction_digits}}})?"
            ));
        }

        let fraction_digits = 29 - integer_digits;
        let prefix = &maximum[..integer_digits];
        let lower_prefix = fixed_width_rate_digits_below(prefix, true);
        let bounded_suffix = fixed_width_rate_digits_at_most(&maximum[integer_digits..]);
        alternatives.push(format!(
            "(?:{lower_prefix}\\.[0-9]{{{fraction_digits}}}|{prefix}\\.{bounded_suffix})"
        ));
    }
    alternatives.push(fixed_width_rate_digits_at_most(maximum));
    format!("^({})$", alternatives.join("|"))
}

fn fixed_width_rate_digits_below(bound: &str, first_digit_nonzero: bool) -> String {
    let mut alternatives = Vec::new();
    for (index, digit) in bound.bytes().enumerate() {
        let minimum = if index == 0 { b'1' } else { b'0' };
        let minimum = if index == 0 && !first_digit_nonzero {
            b'0'
        } else {
            minimum
        };
        if digit > minimum {
            let prefix = &bound[..index];
            let upper = digit - 1;
            let current = if minimum == upper {
                (minimum as char).to_string()
            } else {
                format!("[{}-{}]", minimum as char, upper as char)
            };
            let suffix = if index + 1 == bound.len() {
                String::new()
            } else {
                format!("[0-9]{{{}}}", bound.len() - index - 1)
            };
            alternatives.push(format!("{prefix}{current}{suffix}"));
        }
    }
    format!("(?:{})", alternatives.join("|"))
}

fn fixed_width_rate_digits_at_most(bound: &str) -> String {
    let lower = fixed_width_rate_digits_below(bound, false);
    if lower == "(?:)" {
        bound.to_owned()
    } else {
        format!(
            "(?:{}|{bound})",
            lower.trim_start_matches("(?:").trim_end_matches(')')
        )
    }
}

fn financial_price_scope_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,
        "allOf":[
            {"if":{"properties":{"cloud":{"const":"aws"}},"required":["cloud"]},"then":{"properties":{"launch_facts":{"properties":{"cloud":{"const":"aws"}},"required":["cloud"]}}}},
            {"if":{"properties":{"cloud":{"const":"azure"}},"required":["cloud"]},"then":{"properties":{"launch_facts":{"properties":{"cloud":{"const":"azure"}},"required":["cloud"]}}}},
            {"if":{"properties":{"cloud":{"const":"gcp"}},"required":["cloud"]},"then":{"properties":{"launch_facts":{"properties":{"cloud":{"const":"gcp"}},"required":["cloud"]}}}}
        ],
        "required":["cloud","configured_account","native_account","configured_location","native_location","binding_id","machine_type","os","architecture","image_alias","resolved_image","purchasing","vcpu","memory_gib","gpu_count","boot_disk_class","boot_disk_gib","boot_disk_iops","boot_disk_throughput_mib_s","local_scratch_gib","launch_facts"],
        "properties":{
            "cloud":{"enum":["aws","azure","gcp"]},
            "configured_account":{"type":"string","minLength":1},"native_account":{"type":"string","minLength":1},
            "configured_location":{"type":"string","minLength":1},"native_location":{"type":"string","minLength":1},
            "binding_id":{"type":"string","minLength":1},"machine_type":{"type":"string","minLength":1},
            "os":{"type":"string","minLength":1},"architecture":{"type":"string","minLength":1},
            "image_alias":{"type":"string","minLength":1},"resolved_image":{"type":"string","minLength":1},
            "purchasing":{"type":"string","minLength":1},"vcpu":{"type":"integer","minimum":0},
            "memory_gib":{"type":["number","string"],"minimum":0},"gpu_vendor":{"type":"string"},"gpu_model":{"type":"string"},
            "gpu_count":{"type":"integer","minimum":0},"gpu_memory_gib":{"type":["number","string"],"minimum":0},
            "boot_disk_class":{"type":"string","minLength":1},"boot_disk_gib":{"type":"integer","minimum":0},
            "boot_disk_iops":{"type":"integer","minimum":0},"boot_disk_throughput_mib_s":{"type":["number","string"],"minimum":0},
            "local_scratch_gib":{"type":["number","string"],"minimum":0},"launch_facts":{"$ref":"#/$defs/FinancialLaunchFacts"}
        }
    })
}

fn financial_launch_facts_schema() -> Value {
    json!({
        "oneOf":[
            {"type":"object","additionalProperties":false,"required":["cloud","facts"],"properties":{"cloud":{"const":"aws"},"facts":{"type":"object","additionalProperties":false,"required":["availability_zone","subnet_id","security_group_ids","map_public_ip_on_launch","resolved_image_id","windows_license_included","root_device_name","minimum_root_volume_gib"],"properties":{"availability_zone":{"type":"string"},"subnet_id":{"type":"string"},"security_group_ids":{"type":"array","items":{"type":"string"}},"map_public_ip_on_launch":{"type":"boolean"},"resolved_image_id":{"type":"string"},"windows_license_included":{"type":"boolean"},"root_device_name":{"type":"string"},"minimum_root_volume_gib":{"type":"integer","minimum":0}}}}},
            {"type":"object","additionalProperties":false,"required":["cloud","facts"],"properties":{"cloud":{"const":"azure"},"facts":{"type":"object","additionalProperties":false,"required":["subscription_id","native_location","resolved_image_version","os_disk_sku","minimum_os_disk_gib","image_licensing"],"properties":{"subscription_id":{"type":"string"},"native_location":{"type":"string"},"resolved_image_version":{"type":"string"},"os_disk_sku":{"type":"string"},"public_ip_sku":{"type":"string"},"vnet_subnet_id":{"type":"string"},"minimum_os_disk_gib":{"type":"integer","minimum":0},"image_licensing":{"enum":["no_additional_software_charge","windows_license_in_compute_meter","additional_unpriced_software"]}}}}},
            {"type":"object","additionalProperties":false,"required":["cloud","facts"],"properties":{"cloud":{"const":"gcp"},"facts":{"type":"object","additionalProperties":false,"required":["project_id","native_zone","network_id","subnetwork_id","external_ip_access","resolved_image_id","image_minimum_disk_gib","bundled_local_ssd_partitions","licenses"],"properties":{"project_id":{"type":"string"},"native_zone":{"type":"string"},"network_id":{"type":"string"},"subnetwork_id":{"type":"string"},"external_ip_access":{"type":"boolean"},"resolved_image_id":{"type":"string"},"image_minimum_disk_gib":{"type":"integer","minimum":0},"bundled_local_ssd_partitions":{"type":"integer","minimum":0},"licenses":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["license_uri","basis"],"properties":{"license_uri":{"type":"string"},"basis":{"enum":["included","per_vcpu","per_instance","sql_vcpu_floor"]},"billing_sku":{"type":"string"}}}}}}}}
        ]
    })
}

fn gcp_catalog_binding_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "accelerator_facts": {"additionalProperties":{"$ref":"#/$defs/GcpAcceleratorFacts"},"type":"object"},
            "capacity_advice_enabled": {"default":false,"type":"boolean"},
            "disk_performance": {"additionalProperties":{"$ref":"#/$defs/GcpDiskPerformance"},"type":"object"},
            "external_ip_billing_sku": {"type":["string","null"]},
            "id": {"minLength":1,"type":"string"},
            "images": {"additionalProperties":{"$ref":"#/$defs/GcpImageBinding"},"type":"object"},
            "machine_facts": {"additionalProperties":{"$ref":"#/$defs/GcpMachineFacts"},"type":"object"},
            "provider_binding_id": {"minLength":1,"type":"string"},
            "storage_billing_skus": {"additionalProperties":{"type":"string"},"type":"object"},
            "storage_classes": {"additionalProperties":{"type":"string"},"type":"object"},
        },
        "required": ["id","provider_binding_id","images","storage_classes","storage_billing_skus","accelerator_facts","machine_facts","disk_performance"],
        "type": "object",
    })
}

fn azure_catalog_binding_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "accelerator_facts": {"additionalProperties":{"$ref":"#/$defs/AzureAcceleratorFacts"},"type":"object"},
            "id": {"minLength":1,"type":"string"},
            "images": {"additionalProperties":{"$ref":"#/$defs/AzureImageBinding"},"type":"object"},
            "locations": {"additionalProperties":{"$ref":"#/$defs/AzureNetworkSource"},"type":"object"},
            "provider_binding_id": {"minLength":1,"type":"string"},
            "storage_classes": {"additionalProperties":{"type":"string"},"type":"object"},
        },
        "required": ["id","provider_binding_id","locations","images","storage_classes","accelerator_facts"],
        "type": "object",
    })
}

fn azure_accelerator_facts_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["vendor","model","total_memory_gib","source"],
        "properties":{
            "vendor":{"type":"string","minLength":1},
            "model":{"type":"string","minLength":1},
            "total_memory_gib":{"type":"number","exclusiveMinimum":0},
            "source":{"type":"string","minLength":1}
        }
    })
}

fn azure_network_binding_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "public_ip_sku": {"enum":["Standard",null],"type":["string","null"]},
        },
        "required": [],
        "type": "object",
    })
}

fn azure_image_binding_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["image_id","minimum_os_disk_gib","minimum_os_disk_source","capabilities","licensing","licensing_source"],
        "properties":{
            "image_id":{"type":"string","minLength":1},
            "minimum_os_disk_gib":{"type":"integer","minimum":1},
            "minimum_os_disk_source":{"type":"string","minLength":1},
            "capabilities":{"$ref":"#/$defs/StringSet"},
            "licensing":{"enum":["no_additional_software_charge","windows_license_in_compute_meter","additional_unpriced_software"]},
            "licensing_source":{"type":"string","minLength":1}
        }
    })
}

fn aws_catalog_binding_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "account_id": {"minLength":1,"type":"string"},
            "id": {"minLength":1,"type":"string"},
            "images": {"additionalProperties":{"$ref":"#/$defs/AwsImageBinding"},"type":"object"},
            "locations": {"additionalProperties":{"$ref":"#/$defs/AwsNetworkSource"},"type":"object"},
            "pricing_location": {"minLength":1,"type":"string"},
            "provider_binding_id": {"minLength":1,"type":"string"},
            "storage_classes": {"additionalProperties":{"type":"string"},"type":"object"},
        },
        "required": ["id","provider_binding_id","account_id","pricing_location","locations","images","storage_classes"],
        "type": "object",
    })
}

fn aws_network_binding_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "availability_zone": {"type":"string"},
            "security_group_ids": {"$ref":"#/$defs/StringList"},
        },
        "required": ["availability_zone","security_group_ids"],
        "type": "object",
    })
}

fn aws_image_binding_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["image_id","os","architecture","capabilities","windows_license_included","cloud_init","ec2_launch_agent"],
        "properties":{
            "image_id":{"type":"string"},
            "os":{"type":"string"},
            "architecture":{"type":"string"},
            "capabilities":{"$ref":"#/$defs/StringSet"},
            "windows_license_included":{"type":"boolean"},
            "cloud_init":{"type":"boolean"},
            "ec2_launch_agent":{"type":"boolean"}
        }
    })
}

pub fn extra_specs_json_schema() -> Value {
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "$id":"urn:outrider:pool-extra-specs:v1",
        "title":"Outrider GARM pool extra specs",
        "description":"References a durable Outrider configuration snapshot registered for this controller and pool.",
        "type":"object",
        "required":["snapshot_id"],
        "properties":{"snapshot_id":{"type":"string","minLength":1}},
        "additionalProperties":false
    })
}
