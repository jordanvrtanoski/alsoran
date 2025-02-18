//! ng_setup - the initial handshake that establishes an instance of the NG reference point between GNB and AMF

use super::{GnbCuCp, Workflow};
use anyhow::{anyhow, Result};
use asn1_per::*;
use ngap::*;
use slog::info;
use xxap::Snssai;

impl<'a, G: GnbCuCp> Workflow<'a, G> {
    // Ng Setup Procedure
    // 1.    Connect to the AMF
    // 2.    Ngap NgSetupRequest >>
    // 3.    Ngap NgSetupResponse <<
    pub async fn ng_setup(&self, amf_ip_address: &str) -> Result<()> {
        // Connect to the AMF
        self.gnb_cu_cp
            .ngap_connect(amf_ip_address)
            .await
            .map_err(|_e| anyhow!("Failed to connect to AMF {} (will retry)", amf_ip_address))?;

        // This uses the default expected values of free5GC.
        let ng_setup_request = NgSetupRequest {
            global_ran_node_id: super::build_ngap::build_global_ran_node_id(self.gnb_cu_cp),
            ran_node_name: self.config().name.clone().map(RanNodeName),
            supported_ta_list: SupportedTaList(nonempty![SupportedTaItem {
                tac: Tac([0, 0, 1]),
                broadcast_plmn_list: BroadcastPlmnList(nonempty![BroadcastPlmnItem {
                    plmn_identity: PlmnIdentity(self.config().plmn),
                    tai_slice_support_list: SliceSupportList(nonempty![
                        SliceSupportItem {
                            snssai: Snssai(1, None).into(),
                        },
                        SliceSupportItem {
                            snssai: Snssai(1, Some([0, 0, 0])).into(),
                        },
                        SliceSupportItem {
                            snssai: Snssai(1, Some([0, 0, 1])).into(),
                        }
                    ]),
                    npn_support: None,
                    extended_tai_slice_support_list: None,
                }]),
                configured_tac_indication: None,
                rat_information: None,
            }]),
            default_paging_drx: PagingDrx::V128,
            ue_retention_information: None,
            nb_iot_default_paging_drx: None,
            extended_ran_node_name: None,
        };
        self.log_message("NgSetupRequest >>");
        let response = self
            .ngap_request::<NgSetupProcedure>(ng_setup_request, self.logger)
            .await?;
        self.log_message("NgSetupResponse <<");
        info!(
            self.logger,
            "NGAP interface initialized with {:?}", response.amf_name
        );

        // Associate this TNLA with the NGAP interface instance.
        //self.associate_connection();

        Ok(())
    }
}
