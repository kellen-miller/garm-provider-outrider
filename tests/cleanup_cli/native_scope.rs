use super::cleanup_scenario::{
    CleanupFixture, attestation, fixture, invoke_inspect, invoke_resolve,
};
use serde_json::json;
use std::collections::BTreeMap;

use outrider::{
    allocation::catalog_configuration_fingerprint, cleanup::CleanupInspection,
    github::SnapshotDocument, journal::Journal, policy::CatalogOffer, providers::ProviderBinding,
};

pub(super) fn assert_native_cli_resolution(
    name: &str,
    cloud: NativeCloudScope,
    logical_account: &str,
    native_account: &str,
) {
    let fixture = native_cloud_fixture(name, cloud);
    let output = invoke_inspect(&fixture);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inspection: CleanupInspection = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse native scope inspection: {error}"));
    let expected = native_scope_expectation(cloud);
    assert_eq!(inspection.cloud, expected.cloud);
    assert_eq!(inspection.logical_account_id, logical_account);
    assert_eq!(inspection.native_account_id, native_account);
    assert_eq!(inspection.logical_location, expected.logical_location);
    assert_eq!(inspection.native_location, expected.native_location);
    assert_ne!(inspection.logical_location, inspection.native_location);
    assert_eq!(
        inspection.native_resource_id.as_deref(),
        Some(expected.native_resource_id)
    );
    assert_eq!(inspection.native_host_id.as_deref(), Some("native-host-1"));
    assert_eq!(
        inspection.native_dependencies.get("network_interface"),
        Some(&"native-network-1".to_owned())
    );
    assert_native_launch_scope(&inspection, cloud);
    let proof = attestation(&inspection);
    let resolved = invoke_resolve(&fixture, &proof);
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let audited_scope = read_audited_scope(&fixture);
    assert_eq!(audited_scope["native_account_id"], native_account);
    assert_eq!(audited_scope["native_location"], inspection.native_location);
    assert_eq!(
        audited_scope["native_resource_id"].as_str(),
        inspection.native_resource_id.as_deref()
    );
    assert_eq!(audited_scope["native_host_id"], "native-host-1");
    assert_eq!(
        audited_scope["native_dependencies"]["network_interface"],
        "native-network-1"
    );
    assert_eq!(
        audited_scope["launch_scope"]["image_alias"],
        inspection.launch_scope.image_alias
    );
    assert_eq!(
        audited_scope["launch_scope"]["native_image_id"],
        inspection.launch_scope.native_image_id
    );
}

pub(super) fn assert_native_launch_scope(inspection: &CleanupInspection, cloud: NativeCloudScope) {
    assert_eq!(inspection.launch_scope.image_alias, "linux-alias");
    match cloud {
        NativeCloudScope::Azure => {
            assert_eq!(
                inspection.launch_scope.native_image_id,
                "azure-native-image"
            );
            assert_eq!(
                inspection.launch_scope.vnet_subnet_id.as_deref(),
                Some(
                    "/subscriptions/subscription-Native-123/resourceGroups/rg/providers/Microsoft.Network/virtualNetworks/vnet/subnets/subnet"
                )
            );
            assert!(inspection.launch_scope.network_id.is_none());
        }
        NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => {
            assert_eq!(
                inspection.launch_scope.native_image_id,
                "projects/project-native-456/global/images/linux-native"
            );
            assert_eq!(
                inspection.launch_scope.network_id.as_deref(),
                Some("projects/project-native-456/global/networks/default")
            );
            assert_eq!(
                inspection.launch_scope.subnetwork_id.as_deref(),
                Some("projects/project-native-456/regions/us-central1/subnetworks/default")
            );
        }
    }
}

pub(super) fn read_audited_scope(fixture: &CleanupFixture) -> serde_json::Value {
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read native scope audit: {error}"));
    let summary_json: String = connection
        .query_row(
            "SELECT summary_json FROM cleanup_resolution_audit",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read native scope audit summary: {error}"));
    let summary: serde_json::Value = serde_json::from_str(&summary_json)
        .unwrap_or_else(|error| panic!("parse native scope audit: {error}"));
    summary["inspection"].clone()
}

#[derive(Clone, Copy)]
pub(super) enum NativeCloudScope {
    Azure,
    Gcp,
    GcpBareImage,
}

pub(super) struct NativeScopeExpectation {
    pub(super) cloud: &'static str,
    pub(super) logical_location: &'static str,
    pub(super) native_location: &'static str,
    pub(super) native_resource_id: &'static str,
}

pub(super) fn native_scope_expectation(cloud: NativeCloudScope) -> NativeScopeExpectation {
    match cloud {
        NativeCloudScope::Azure => NativeScopeExpectation {
            cloud: "azure",
            logical_location: "east-us-alias",
            native_location: "eastus2",
            native_resource_id: "/subscriptions/subscription-Native-123/resourceGroups/rg/providers/Microsoft.Compute/virtualMachines/runner-1",
        },
        NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => NativeScopeExpectation {
            cloud: "gcp",
            logical_location: "US Central pricing",
            native_location: "us-central1-a",
            native_resource_id: "projects/project-native-456/zones/us-central1-a/instances/runner-1",
        },
    }
}

pub(super) fn native_cloud_fixture(name: &str, cloud: NativeCloudScope) -> CleanupFixture {
    let mut fixture = fixture(name);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open native cloud fixture: {error}"));
    let mut snapshot_json: String = connection
        .query_row(
            "SELECT config_json FROM pool_snapshots WHERE id='snapshot-1'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read native cloud snapshot: {error}"));
    let mut offer_json: String = connection
        .query_row(
            "SELECT selected_offer_json FROM allocations WHERE allocation_id='allocation-1'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read native cloud offer: {error}"));
    let mut binding_json: String = connection
        .query_row(
            "SELECT binding_json FROM allocations WHERE allocation_id='allocation-1'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read native cloud provider pin: {error}"));
    let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot_json)
        .unwrap_or_else(|error| panic!("parse native cloud snapshot: {error}"));
    let mut offer: serde_json::Value = serde_json::from_str(&offer_json)
        .unwrap_or_else(|error| panic!("parse native cloud offer: {error}"));
    let mut binding: serde_json::Value = serde_json::from_str(&binding_json)
        .unwrap_or_else(|error| panic!("parse native cloud provider pin: {error}"));
    let (cloud_name, logical_account, logical_location, native_account, native_location) =
        match cloud {
            NativeCloudScope::Azure => (
                "azure",
                "logical-azure-account",
                "east-us-alias",
                "subscription-Native-123",
                "eastus2",
            ),
            NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => (
                "gcp",
                "project-native-456",
                "US Central pricing",
                "project-native-456",
                "us-central1-a",
            ),
        };
    binding["cloud"] = json!(cloud_name);
    binding["account"] = json!(logical_account);
    binding["location"] = json!(logical_location);
    snapshot["central"]["provider_bindings"][0] = binding.clone();
    snapshot["central"]["aws_catalog_bindings"] = serde_json::Value::Null;
    match cloud {
        NativeCloudScope::Azure => {
            snapshot["central"]["azure_catalog_bindings"] = json!([{
                "id":"catalog-1",
                "provider_binding_id":"aws-pin",
                "account_id":logical_account,
                "subscription_id":native_account,
                "pricing_location":"Azure East",
                "credential_reference":"azure-prod",
                "locations":{
                    logical_location:{
                        "location":native_location,
                        "vnet_subnet_id":"/subscriptions/subscription-Native-123/resourceGroups/rg/providers/Microsoft.Network/virtualNetworks/vnet/subnets/subnet",
                        "public_ip_sku":null
                    }
                },
                "images":{
                    "linux-alias":{
                        "image_id":"azure-native-image",
                        "minimum_os_disk_gib":20,
                        "minimum_os_disk_source":"catalog",
                        "capabilities":[],
                        "licensing":"no_additional_software_charge",
                        "licensing_source":"catalog"
                    }
                }, "storage_classes":{}, "accelerator_facts":{}
            }]);
            snapshot["central"]["gcp_catalog_bindings"] = serde_json::Value::Null;
            offer["candidate"]["identity"] =
                json!("azure/logical-azure-account/east-us-alias/Standard_D2s_v5");
            offer["candidate"]["cloud"] = json!(cloud_name);
            offer["candidate"]["account"] = json!(logical_account);
            offer["candidate"]["location"] = json!(logical_location);
            offer["candidate"]["image"] = json!("linux-alias");
            offer["selection"]["image"] = json!("azure-native-image");
            offer["launch_facts"] = json!({
                "provider":"azure", "facts":{
                    "subscription_id":native_account,
                    "location":native_location,
                    "image_version":"latest", "os_disk_sku":"Premium_LRS",
                    "public_ip_sku":null, "minimum_os_disk_gib":20,
                    "minimum_os_disk_source":"catalog", "image_licensing":"no_additional_software_charge",
                    "image_licensing_source":"catalog", "maximum_data_disk_count":null,
                    "maximum_os_disk_iops":null, "maximum_os_disk_throughput_mib_s":null,
                    "vm_size_capabilities":{}, "ephemeral_os_disk_supported":false
                }
            });
        }
        NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => {
            let configured_image_id = match cloud {
                NativeCloudScope::GcpBareImage => "linux-native",
                NativeCloudScope::Gcp | NativeCloudScope::Azure => {
                    "projects/project-native-456/global/images/linux-native"
                }
            };
            snapshot["central"]["azure_catalog_bindings"] = serde_json::Value::Null;
            snapshot["central"]["gcp_catalog_bindings"] = json!([{
                "id":"catalog-1", "provider_binding_id":"aws-pin",
                "project_id":native_account, "zone":native_location,
                "pricing_location":logical_location, "credential_reference":"gcp-prod",
                "images":{
                    "linux-alias":{
                        "image_id":configured_image_id,
                        "os":"linux", "os_source":"catalog", "architecture":"x86_64",
                        "capabilities":[], "capabilities_source":"catalog",
                        "license_free_source":null, "license_facts":{}
                    }
                }, "storage_classes":{}, "storage_billing_skus":{},
                "external_ip_billing_sku":null, "accelerator_facts":{}, "machine_facts":{},
                "disk_performance":{}, "capacity_advice_enabled":false
            }]);
            offer["candidate"]["identity"] =
                json!("gcp/project-native-456/US Central pricing/e2-standard-2");
            offer["candidate"]["cloud"] = json!(cloud_name);
            offer["candidate"]["account"] = json!(logical_account);
            offer["candidate"]["location"] = json!(logical_location);
            offer["candidate"]["image"] = json!("linux-alias");
            offer["selection"]["image"] = json!(configured_image_id);
            offer["launch_facts"] = json!({
                "provider":"gcp", "facts":{
                    "project_id":native_account, "zone":native_location,
                    "network_id":"projects/project-native-456/global/networks/default",
                    "subnetwork_id":"projects/project-native-456/regions/us-central1/subnetworks/default",
                    "external_ip_access":false, "image_id":"projects/project-native-456/global/images/linux-native",
                    "image_minimum_disk_gib":20, "machine_type_accelerators":[],
                    "bundled_local_ssd_partitions":0
                }
            });
        }
    }
    let snapshot: SnapshotDocument = serde_json::from_value(snapshot)
        .unwrap_or_else(|error| panic!("deserialize native cloud snapshot: {error}"));
    let offer: CatalogOffer = serde_json::from_value(offer)
        .unwrap_or_else(|error| panic!("deserialize native cloud offer: {error}"));
    let binding: ProviderBinding = serde_json::from_value(binding)
        .unwrap_or_else(|error| panic!("deserialize native cloud pin: {error}"));
    let catalog_fingerprint =
        catalog_configuration_fingerprint(&snapshot.central, cloud_name, "catalog-1")
            .unwrap_or_else(|error| panic!("fingerprint native cloud catalog: {error}"));
    let native_identity = outrider::cloud::PlacementIdentity {
        controller_id: "controller-1".into(),
        cloud: cloud_name.into(),
        account_id: match cloud {
            NativeCloudScope::Azure => "SUBSCRIPTION-NATIVE-123".into(),
            NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => native_account.into(),
        },
        location: match cloud {
            NativeCloudScope::Azure => "EastUS2".into(),
            NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => native_location.into(),
        },
        resource_id: match cloud {
            NativeCloudScope::Azure => "/subscriptions/subscription-Native-123/resourceGroups/rg/providers/Microsoft.Compute/virtualMachines/runner-1".into(),
            NativeCloudScope::Gcp | NativeCloudScope::GcpBareImage => "projects/project-native-456/zones/us-central1-a/instances/runner-1".into(),
        },
        host_id: Some("native-host-1".into()),
        dependency_ids: BTreeMap::from([("network_interface".into(), "native-network-1".into())]),
    };
    snapshot_json = serde_json::to_string(&snapshot)
        .unwrap_or_else(|error| panic!("serialize native cloud snapshot: {error}"));
    offer_json = serde_json::to_string(&offer)
        .unwrap_or_else(|error| panic!("serialize native cloud offer: {error}"));
    binding_json = serde_json::to_string(&binding)
        .unwrap_or_else(|error| panic!("serialize native cloud pin: {error}"));
    let account_scope =
        outrider::allocation::canonical_native_account_scope(cloud_name, native_account)
            .unwrap_or_else(|error| panic!("canonicalize native account scope: {error}"));
    connection
        .execute(
            "UPDATE pool_snapshots SET config_json=?2 WHERE id=?1",
            rusqlite::params!["snapshot-1", snapshot_json],
        )
        .unwrap_or_else(|error| panic!("update native cloud snapshot: {error}"));
    connection
        .execute(
            "UPDATE allocations SET selected_offer_json=?2, binding_json=?3, account_id=?4, catalog_fingerprint=?5, native_identity_json=?6 WHERE allocation_id=?1",
            rusqlite::params!["allocation-1", offer_json, binding_json, account_scope, catalog_fingerprint, serde_json::to_string(&native_identity).unwrap_or_else(|error| panic!("serialize native identity: {error}"))],
        )
        .unwrap_or_else(|error| panic!("update native cloud allocation: {error}"));
    connection
        .execute(
            "UPDATE child_attempts SET offer_json=?2, binding_json=?3 WHERE allocation_id=?1",
            rusqlite::params!["allocation-1", offer_json, binding_json],
        )
        .unwrap_or_else(|error| panic!("update native cloud child attempt: {error}"));
    drop(connection);
    if !matches!(cloud, NativeCloudScope::GcpBareImage) {
        let journal = Journal::open(&fixture.temp.path)
            .unwrap_or_else(|error| panic!("inspect native cloud journal: {error}"));
        fixture.inspection = outrider::cleanup::inspect(&journal, "controller-1", "allocation-1")
            .unwrap_or_else(|error| panic!("inspect native cloud fixture: {error}"));
    }
    fixture
}
