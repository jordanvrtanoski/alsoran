//! add_e1ap_endpoint - Adds an E1AP endpoint

use super::Workflow;
use crate::gnb_cu_cp::GnbCuCp;
use anyhow::Result;
use asn1_per::*;
use f1ap::{
    CpTransportLayerAddress, GnbCuConfigurationUpdate, GnbCuConfigurationUpdateProcedure,
    GnbCuTnlAssociationToAddItem, GnbCuTnlAssociationToAddList, TnlAssociationUsage, TransactionId,
};

impl<'a, G: GnbCuCp> Workflow<'a, G> {
    pub async fn gnb_cu_configuration_update(&self, f1ap_endpoint_ip_addr: &str) -> Result<()> {
        let gnb_cu_configuration_update = GnbCuConfigurationUpdate {
            transaction_id: TransactionId(1), // TODO
            cells_to_be_activated_list: None,
            cells_to_be_deactivated_list: None,
            gnb_cu_tnl_association_to_add_list: Some(GnbCuTnlAssociationToAddList(nonempty![
                GnbCuTnlAssociationToAddItem {
                    tnl_association_transport_layer_address:
                        CpTransportLayerAddress::EndpointIpAddress(
                            f1ap_endpoint_ip_addr.try_into()?,
                        ),
                    tnl_association_usage: TnlAssociationUsage::Both,
                },
            ])),
            gnb_cu_tnl_association_to_remove_list: None,
            gnb_cu_tnl_association_to_update_list: None,
            cells_to_be_barred_list: None,
            protected_eutra_resources_list: None,
            neighbour_cell_information_list: None,
            transport_layer_address_info: None,
            ul_bh_non_up_traffic_mapping: None,
            bap_address: None,
        };

        self.log_message("<< GnbCuConfigurationUpdate");
        let _response = self
            .f1ap_request::<GnbCuConfigurationUpdateProcedure>(
                gnb_cu_configuration_update,
                self.logger,
            )
            .await?;
        self.log_message(">> GnbCuConfigurationUpdateAcknowledge");

        // Associate this TNLA with the F1AP interface instance.
        // It is essential to spawn this, not await it, to avoid a deadlock
        // with the coordinator.  (The coordinator is already waiting on us, so it can't
        // process our next message to it until we have returned.)
        async_std::task::spawn(self.associate_connection());

        Ok(())
    }
}
