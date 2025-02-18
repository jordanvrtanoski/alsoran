//! worker - the top level struct for a gNB-CU-CP worker, which implements the GnbCuCp trait

use super::config::ConnectionStyle;
use super::datastore::{UeState, UeStateStore};
use super::handlers::RrcHandler;
use super::rrc_transaction::{PendingRrcTransactions, RrcTransaction};
use super::Config;
use crate::datastore::StateStore;
use crate::handlers::connection_api::ConnectionApiHandler;
use crate::handlers::{E1apHandler, F1apHandler, NgapHandler};
use crate::{GnbCuCp, WorkerConnectionManagementConfig};
use anyhow::Result;
use async_channel::Sender;
use async_std::future;
use async_std::sync::Mutex;
use async_trait::async_trait;
use coordination_api::models::{ConnectionState, RefreshWorker, WorkerInfo};
use coordination_api::{
    Api as CoordinationApi, Client as CoordinationApiClient, RefreshWorkerResponse,
};
use coordinator::Coordinator;
use f1ap::{DlRrcMessageTransfer, DlRrcMessageTransferProcedure, GnbCuUeF1apId, SrbId};
use net::{
    Indication, IndicationHandler, Procedure, RequestError, RequestProvider, SctpTransportProvider,
    ShutdownHandle, Stack,
};
use rrc::UlDcchMessage;
use slog::{debug, info, warn, Logger};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use stop_token::{StopSource, StopToken};
use swagger::{ApiError, AuthData, ContextBuilder, EmptyContext, Push, XSpanIdString};
use uuid::Uuid;

pub type ClientContext = swagger::make_context_ty!(
    ContextBuilder,
    EmptyContext,
    Option<AuthData>,
    XSpanIdString
);
#[derive(Clone)]
pub struct Worker<A: CoordinationApi<ClientContext>, U: UeStateStore> {
    worker_id: Uuid,
    config: Config,
    ngap: Stack,
    f1ap: Stack,
    e1ap: Stack,
    ue_store: U,
    coordinator: A,
    logger: Logger,
    rrc_transactions: PendingRrcTransactions,
    shutdown_handles: Arc<Mutex<Vec<ShutdownHandle>>>,
}

// TS38.412, 7
// The Payload Protocol Identifier (ppid) assigned by IANA to be used by SCTP for the application layer protocol NGAP
// is 60, and 66 for DTLS over SCTP (IETF RFC 6083 [8]).
const NGAP_SCTP_PPID: u32 = 60;
const NGAP_BIND_PORT: u16 = 38412;

// TS38.472, 7
// The Payload Protocol Identifier (ppid) assigned by IANA to be used by SCTP for the application layer protocol F1AP is 62,
// and 68 for DTLS over SCTP (IETF RFC 6083 [9]).
const F1AP_SCTP_PPID: u32 = 62;
const F1AP_BIND_PORT: u16 = 38472;

// TS38.462
const E1AP_SCTP_PPID: u32 = 64;
const E1AP_BIND_PORT: u16 = 38462;

pub async fn spawn<U: UeStateStore>(
    worker_id: Uuid,
    config: Config,
    ue_store: U,
    logger: Logger,
) -> Result<ShutdownHandle> {
    let stop_source = StopSource::new();
    let stop_token = stop_source.token();

    info!(&logger, "Starting gNB-CU-CP worker {}", worker_id);
    info!(&logger, "PLMN is {:02x?}", config.plmn);
    debug!(&logger, "Config: {:?}", config);

    let handle = match config.connection_style {
        // Run a combined worker and coordinator.
        ConnectionStyle::Autonomous(ref connection_control_config) => {
            let (coordinator, receiver) = Coordinator::new(logger.clone());
            let worker = Worker::new(
                config.clone(),
                ue_store,
                worker_id,
                logger.clone(),
                coordinator.clone(),
            );
            let handler = ConnectionApiHandler::new(worker.clone(), logger);
            let coordinator_shutdown_handle = coordinator.start_with_local_api_provider(
                connection_control_config.clone(),
                receiver,
                handler,
            );
            async_std::task::spawn(async move {
                worker
                    .serve(stop_token)
                    .await
                    .expect("Worker startup failure");
                coordinator_shutdown_handle.graceful_shutdown().await;
            })
        }

        // Run a worker and serve the connection API so that it can be managed by the coordinator.
        ConnectionStyle::Coordinated(ref worker_connection_management_config) => {
            let coordinator = CoordinationApiClient::try_new_http(
                &worker_connection_management_config.coordinator_base_path,
            )
            .unwrap();
            let worker = Worker::new(config, ue_store, worker_id, logger, coordinator);
            worker.start_servers().await?;
            async_std::task::spawn(async move {
                worker.run(stop_token).await;
            })
        }
    };
    Ok(ShutdownHandle::new(handle, stop_source))
}

impl<A: Clone + Send + Sync + 'static + CoordinationApi<ClientContext>, U: UeStateStore>
    Worker<A, U>
{
    fn new(
        config: Config,
        ue_store: U,
        worker_id: Uuid,
        logger: Logger,
        coordinator: A,
    ) -> Worker<A, U> {
        Worker {
            worker_id,
            config,
            ngap: Stack::new(SctpTransportProvider::new()),
            f1ap: Stack::new(SctpTransportProvider::new()),
            e1ap: Stack::new(SctpTransportProvider::new()),
            ue_store,
            coordinator,
            logger,
            rrc_transactions: PendingRrcTransactions::new(),
            shutdown_handles: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn start_servers(&self) -> Result<()> {
        let f1ap_handle = self.serve_f1ap().await?;
        self.add_shutdown_handle(f1ap_handle).await;

        let e1ap_handle = self.serve_e1ap().await?;
        self.add_shutdown_handle(e1ap_handle).await;

        if let ConnectionStyle::Coordinated(WorkerConnectionManagementConfig {
            connection_api_bind_port,
            ..
        }) = self.config.connection_style
        {
            let connection_api_handle = self.serve_connection_api(connection_api_bind_port).await?;
            self.add_shutdown_handle(connection_api_handle).await;
        };
        Ok(())
    }

    async fn run(self, stop_token: StopToken) {
        // Connect to the coordinator.  It will bring this worker into service by making calls to the
        // connection API.
        self.send_periodic_refreshes_to_coordinator(stop_token.clone())
            .await;

        stop_token.await;

        while let Some(item) = self.shutdown_handles.lock().await.pop() {
            item.graceful_shutdown().await;
        }

        self.ngap.graceful_shutdown().await;
    }

    async fn serve(self, stop_token: StopToken) -> Result<()> {
        self.start_servers().await?;
        self.run(stop_token).await;
        Ok(())
    }

    async fn send_periodic_refreshes_to_coordinator(&self, stop_token: StopToken) {
        let clone = self.clone();
        async_std::task::spawn(async move {
            let interval_secs = 10; // TODO - make configurable

            loop {
                let stop_token_clone = stop_token.clone();
                if let Err(e) = clone.send_refresh_worker().await {
                    warn!(clone.logger, "Failed refresh worker - {}", e);
                }
                if future::timeout(Duration::from_secs(interval_secs), stop_token_clone)
                    .await
                    .is_ok()
                {
                    break;
                }
            }
        });
    }

    async fn send_refresh_worker(&self) -> Result<RefreshWorkerResponse, ApiError> {
        let context: ClientContext = swagger::make_context!(
            ContextBuilder,
            EmptyContext,
            None as Option<AuthData>,
            XSpanIdString::default()
        );

        let ng_up = !self.ngap.remote_tnla_addresses().await.is_empty();
        let f1_up = !self.f1ap.remote_tnla_addresses().await.is_empty();
        let e1_up = !self.e1ap.remote_tnla_addresses().await.is_empty();

        let connection_api_url = match &self.config.connection_style {
            ConnectionStyle::Autonomous(_) => "".to_string(),
            ConnectionStyle::Coordinated(WorkerConnectionManagementConfig {
                connection_api_base_path,
                ..
            }) => connection_api_base_path.clone(),
        };

        let worker_ip = self.config.ip_addr.to_string();

        self.coordinator
            .refresh_worker(
                RefreshWorker {
                    worker_id: self.worker_id,
                    worker_info: WorkerInfo {
                        connection_api_url,
                        f1_address: worker_ip.clone(),
                        e1_address: worker_ip,
                    },
                    connection_state: ConnectionState {
                        ng_up,
                        f1_up,
                        e1_up,
                    },
                },
                &context,
            )
            .await
    }

    async fn serve_f1ap(&self) -> Result<ShutdownHandle> {
        let f1_listen_address = self.worker_listen_address(F1AP_BIND_PORT);
        info!(
            &self.logger,
            "Listen for connection from DU on {}", f1_listen_address
        );

        let rrc_handler = RrcHandler::new(self.clone());
        self.f1ap
            .listen(
                f1_listen_address,
                F1AP_SCTP_PPID,
                F1apHandler::new_f1ap_application(self.clone(), rrc_handler),
                self.logger.clone(),
            )
            .await
    }

    async fn serve_e1ap(&self) -> Result<ShutdownHandle> {
        let e1_listen_address = self.worker_listen_address(E1AP_BIND_PORT);
        info!(
            &self.logger,
            "Listen for connection from CU-UP on {}", e1_listen_address
        );

        self.e1ap
            .listen(
                e1_listen_address,
                E1AP_SCTP_PPID,
                E1apHandler::new_e1ap_application(self.clone()),
                self.logger.clone(),
            )
            .await
    }

    async fn serve_connection_api(&self, port: u16) -> Result<ShutdownHandle> {
        let connection_api_listen_address = self.worker_listen_address(port);
        info!(
            &self.logger,
            "Serve connection API on {connection_api_listen_address}",
        );
        let addr = connection_api_listen_address.parse()?;
        crate::handlers::connection_api::serve(addr, self.clone(), self.logger.clone()).await
    }

    fn worker_listen_address(&self, port: u16) -> String {
        format!("{}:{}", self.config.ip_addr, port)
    }

    async fn add_shutdown_handle(&self, shutdown_handle: ShutdownHandle) {
        self.shutdown_handles.lock().await.push(shutdown_handle);
    }
}

#[async_trait]
impl<A: Clone + Send + Sync + 'static + CoordinationApi<ClientContext>, U: UeStateStore>
    StateStore<UeState> for Worker<A, U>
{
    async fn store(&self, k: u32, s: UeState, ttl_secs: usize) -> Result<()> {
        self.ue_store.store(k, s, ttl_secs).await
    }
    async fn retrieve(&self, k: &u32) -> Result<UeState> {
        self.ue_store.retrieve(k).await
    }
    async fn delete(&self, k: &u32) -> Result<()> {
        self.ue_store.delete(k).await
    }
}
impl<A: Clone + Send + Sync + 'static + CoordinationApi<ClientContext>, U: UeStateStore>
    UeStateStore for Worker<A, U>
{
}

#[async_trait]
impl<A: Clone + Send + Sync + 'static + CoordinationApi<ClientContext>, U: UeStateStore> GnbCuCp
    for Worker<A, U>
{
    fn config(&self) -> &Config {
        &self.config
    }
    async fn ngap_connect(&self, amf_ip_address: &str) -> Result<()> {
        let amf_address = format!("{}:{}", amf_ip_address, NGAP_BIND_PORT);
        debug!(&self.logger, "Connect to AMF {}", amf_address);
        self.ngap
            .connect(
                &amf_address,
                &self.config.ip_addr.to_string(),
                NGAP_SCTP_PPID,
                NgapHandler::new_ngap_application(self.clone()),
                self.logger.clone(),
            )
            .await?;
        Ok(())
    }

    async fn ngap_request<P: Procedure>(
        &self,
        r: P::Request,
        logger: &Logger,
    ) -> Result<P::Success, RequestError<P::Failure>> {
        <Stack as RequestProvider<P>>::request(&self.ngap, r, logger)
            .await
            .map(|(x, _)| x)
    }
    async fn ngap_indication<P: Indication>(&self, r: P::Request, logger: &Logger) {
        <Stack as IndicationHandler<P>>::handle(&self.ngap, r, logger).await
    }

    async fn f1ap_request<P: Procedure>(
        &self,
        r: P::Request,
        logger: &Logger,
    ) -> Result<P::Success, RequestError<P::Failure>> {
        <Stack as RequestProvider<P>>::request(&self.f1ap, r, logger)
            .await
            .map(|(x, _)| x)
    }
    async fn f1ap_indication<P: Indication>(&self, r: P::Request, logger: &Logger) {
        <Stack as IndicationHandler<P>>::handle(&self.f1ap, r, logger).await
    }

    async fn e1ap_request<P: Procedure>(
        &self,
        r: P::Request,
        logger: &Logger,
    ) -> Result<P::Success, RequestError<P::Failure>> {
        <Stack as RequestProvider<P>>::request(&self.e1ap, r, logger)
            .await
            .map(|(x, _)| x)
    }
    async fn e1ap_indication<P: Indication>(&self, r: P::Request, logger: &Logger) {
        <Stack as IndicationHandler<P>>::handle(&self.e1ap, r, logger).await
    }

    /// Start a new RRC transaction.
    async fn new_rrc_transaction(&self, ue: &UeState) -> RrcTransaction {
        self.rrc_transactions.new_transaction(ue.key).await
    }

    /// Determine if this is a response to a local pending RRC transaction.
    async fn match_rrc_transaction(&self, ue_id: u32) -> Option<Sender<UlDcchMessage>> {
        // This is not a robust mechanism.  The calling task is only interested in the next matching
        // response to the RRC transactions it initiates, whereas we are giving it the next UlDcchMessage of any kind.
        // TODO
        self.rrc_transactions.match_transaction(ue_id).await
    }

    async fn send_rrc_to_ue(
        &self,
        ue: &UeState,
        srb_id: SrbId,
        rrc_container: f1ap::RrcContainer,
        logger: &Logger,
    ) {
        let dl_message = DlRrcMessageTransfer {
            gnb_cu_ue_f1ap_id: GnbCuUeF1apId(ue.key),
            gnb_du_ue_f1ap_id: ue.gnb_du_ue_f1ap_id,
            old_gnb_du_ue_f1ap_id: None,
            srb_id,
            execute_duplication: None,
            rrc_container,
            rat_frequency_priority_information: None,
            rrc_delivery_status_request: None,
            ue_context_not_retrievable: None,
            redirected_rrc_message: None,
            plmn_assistance_info_for_net_shar: None,
            new_gnb_cu_ue_f1ap_id: None,
            additional_rrm_priority_index: None,
        };

        debug!(&logger, "<< DlRrcMessageTransfer");
        DlRrcMessageTransferProcedure::call_provider(&self.f1ap, dl_message, logger).await
    }

    fn associate_connection(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // The basic initial implementation of this function just sends a refresh to the coordinator and assumes
        // that there is one instance of E1AP, F1AP, and NGAP.  This has the necessary effect of triggrering
        // the coordinator to add all worker endpoints, but will need to be improved a) when we simultaneously
        // support multiple different interface instances or b) if we want to deal with rogue connections that are
        // not properly initialized according to the protocol procedures.
        let self_clone = self.clone();
        let future = async move {
            debug!(self_clone.logger, "Send refresh worker");
            if let Err(e) = self_clone.send_refresh_worker().await {
                warn!(self_clone.logger, "Failed refresh worker {}", e);
            }
        };
        Box::pin(future)
    }
}
