use std::time::Duration;

use outrider::cloud::inventory::{CloudInventory, InventoryKind, InventoryOwnership};
use tokio::time::Instant;

use super::cloud_server::{aws_backend, identity_xml, start_aws_contract_server, xml_response};

#[tokio::test]
async fn native_inventory_is_unfiltered_and_strips_tag_values() {
    let responses = [
        (
            InventoryKind::VirtualMachine,
            "<DescribeInstancesResponse><reservationSet><item><ownerId>synthetic-account</ownerId><instancesSet><item><instanceId>i-abc123</instanceId><blockDeviceMapping><item><ebs><volumeId>vol-abc123</volumeId></ebs></item></blockDeviceMapping><networkInterfaceSet><item><networkInterfaceId>eni-abc123</networkInterfaceId></item></networkInterfaceSet><tagSet><item><key>GARM_CONTROLLER_ID</key><value>controller-1</value></item><item><key>Name</key><value>never-export-this</value></item></tagSet></item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        ),
        (
            InventoryKind::Disk,
            "<DescribeVolumesResponse><volumeSet><item><volumeId>vol-abc123</volumeId><status>available</status></item></volumeSet></DescribeVolumesResponse>",
        ),
        (
            InventoryKind::AllocatedAddress,
            "<DescribeAddressesResponse><addressesSet><item><allocationId>eipalloc-abc123</allocationId><networkInterfaceId>eni-abc123</networkInterfaceId><tagSet><item><key>GARM_CONTROLLER_ID</key><value>never-export-this</value></item></tagSet></item></addressesSet></DescribeAddressesResponse>",
        ),
    ];
    for (kind, xml) in responses {
        let server = start_aws_contract_server(vec![identity_xml(), xml_response(xml)])
            .await
            .unwrap();
        let backend = aws_backend(&server.endpoint).await.unwrap();
        let result = backend
            .inventory(
                kind,
                "controller-1",
                Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert_eq!(result.resources.len(), 1);
        let expected_owner = match kind {
            InventoryKind::VirtualMachine => InventoryOwnership::Controller,
            InventoryKind::Disk => InventoryOwnership::Untagged,
            InventoryKind::AllocatedAddress => InventoryOwnership::OtherController,
        };
        assert_eq!(result.resources[0].ownership, expected_owner);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("never-export-this")
        );
        if kind == InventoryKind::VirtualMachine {
            assert_eq!(
                result.resources[0].dependencies,
                ["vol-abc123", "eni-abc123"]
            );
        }

        if kind == InventoryKind::AllocatedAddress {
            assert_eq!(result.resources[0].attached_to, ["eni-abc123"]);
        }

        let requests = server.requests.lock().unwrap();
        assert!(!requests[1].body.contains("Filter"));
        assert!(requests[1].body.contains("Action=Describe"));
        server.task.abort();
    }
}

#[tokio::test]
async fn volume_inventory_rejects_repeated_pagination_and_denied_second_page() {
    for denied in [false, true] {
        let page = "<DescribeVolumesResponse><volumeSet/><nextToken>next</nextToken></DescribeVolumesResponse>";
        let final_response = if denied {
            let mut response = xml_response(
                "<Response><Errors><Error><Code>UnauthorizedOperation</Code></Error></Errors></Response>",
            );
            response.status = 403;
            response
        } else {
            xml_response(page)
        };
        let server =
            start_aws_contract_server(vec![identity_xml(), xml_response(page), final_response])
                .await
                .unwrap();
        let backend = aws_backend(&server.endpoint).await.unwrap();
        assert!(
            backend
                .inventory(
                    InventoryKind::Disk,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].body.contains("NextToken=next"));
        server.task.abort();
    }
}

#[tokio::test]
async fn inventory_rejects_foreign_accounts_and_missing_native_ids() {
    for xml in [
        "<DescribeInstancesResponse><reservationSet><item><ownerId>foreign</ownerId><instancesSet><item><instanceId>i-abc123</instanceId></item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
        "<DescribeInstancesResponse><reservationSet><item><ownerId>synthetic-account</ownerId><instancesSet><item/></instancesSet></item></reservationSet></DescribeInstancesResponse>",
    ] {
        let server = start_aws_contract_server(vec![identity_xml(), xml_response(xml)])
            .await
            .unwrap();
        let backend = aws_backend(&server.endpoint).await.unwrap();
        assert!(
            backend
                .inventory(
                    InventoryKind::VirtualMachine,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
        server.task.abort();
    }
}
