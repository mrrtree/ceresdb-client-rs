use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use ceresdbproto::{
    common::ResponseHeader,
    storage::{
        storage_service_client::StorageServiceClient, QueryRequest as QueryRequestPb,
        QueryResponse as QueryResponsePb, RouteRequest as RouteRequestPb,
        RouteResponse as RouteResponsePb, WriteRequest as WriteRequestPb,
        WriteResponse as WriteResponsePb,
    },
};
use tonic::{
    metadata::{Ascii, MetadataValue},
    service::Interceptor,
    transport::{Channel, Endpoint},
    Request, Status,
};

use crate::{
    errors::{AuthCode, AuthFailStatus, Error, Result, ServerError},
    options::{RpcConfig, RpcOptions},
    rpc_client::{RpcClient, RpcClientFactory, RpcContext},
    util::is_ok,
};

struct RpcClientImpl {
    channel: Channel,
    default_read_timeout: Duration,
    default_write_timeout: Duration,
}

impl RpcClientImpl {
    fn new(
        channel: Channel,
        default_read_timeout: Duration,
        default_write_timeout: Duration,
    ) -> Self {
        Self {
            channel,
            default_read_timeout,
            default_write_timeout,
        }
    }

    fn check_status(header: ResponseHeader) -> Result<()> {
        if !is_ok(header.code) {
            return Err(Error::Server(ServerError {
                code: header.code,
                msg: header.error,
            }));
        }

        Ok(())
    }

    fn make_request<T>(ctx: &RpcContext, req: T, default_timeout: Duration) -> Request<T> {
        let timeout = ctx.timeout.unwrap_or(default_timeout);
        let mut req = Request::new(req);
        req.set_timeout(timeout);
        req
    }

    fn make_query_request<T>(&self, ctx: &RpcContext, req: T) -> Request<T> {
        Self::make_request(ctx, req, self.default_read_timeout)
    }

    fn make_write_request<T>(&self, ctx: &RpcContext, req: T) -> Request<T> {
        Self::make_request(ctx, req, self.default_write_timeout)
    }
}

#[async_trait]
impl RpcClient for RpcClientImpl {
    async fn query(&self, ctx: &RpcContext, req: QueryRequestPb) -> Result<QueryResponsePb> {
        let interceptor = AuthInterceptor::new(ctx)?;
        let mut client =
            StorageServiceClient::<Channel>::with_interceptor(self.channel.clone(), interceptor);

        let resp = client
            .query(self.make_query_request(ctx, req))
            .await
            .map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }

    async fn write(&self, ctx: &RpcContext, req: WriteRequestPb) -> Result<WriteResponsePb> {
        let interceptor = AuthInterceptor::new(ctx)?;
        let mut client =
            StorageServiceClient::<Channel>::with_interceptor(self.channel.clone(), interceptor);

        let resp = client
            .write(self.make_write_request(ctx, req))
            .await
            .map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }

    async fn route(&self, ctx: &RpcContext, req: RouteRequestPb) -> Result<RouteResponsePb> {
        let interceptor = AuthInterceptor::new(ctx)?;
        let mut client =
            StorageServiceClient::<Channel>::with_interceptor(self.channel.clone(), interceptor);

        // use the write timeout for the route request.
        let route_req = Self::make_request(ctx, req, self.default_write_timeout);
        let resp = client.route(route_req).await.map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }
}

const RPC_HEADER_TENANT_KEY: &str = "x-ceresdb-access-tenant";

/// AuthInterceptor is implemented as an interceptor for tonic.
/// Its duty is to check user authentication.
pub struct AuthInterceptor {
    tenant: MetadataValue<Ascii>,
    _token: MetadataValue<Ascii>,
}

impl AuthInterceptor {
    fn new(ctx: &RpcContext) -> std::result::Result<Self, Error> {
        Ok(AuthInterceptor {
            tenant: ctx.tenant.parse().map_err(|_e| {
                Error::AuthFail(AuthFailStatus {
                    code: AuthCode::InvalidTenantMeta,
                    msg: format!(
                        "invalid tenant: {}, can not be converted to grpc metadata",
                        ctx.tenant
                    ),
                })
            })?,
            _token: ctx.token.parse().map_err(|_e| {
                Error::AuthFail(AuthFailStatus {
                    code: AuthCode::InvalidTokenMeta,
                    msg: format!(
                        "invalid token: {}, can not be converted to grpc metadata",
                        ctx.token
                    ),
                })
            })?,
        })
    }
}

impl Interceptor for AuthInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, Status> {
        request
            .metadata_mut()
            .insert(RPC_HEADER_TENANT_KEY, self.tenant.clone());
        Ok(request)
    }
}

pub struct RpcClientImplFactory {
    rpc_opts: RpcOptions,
    grpc_config: RpcConfig,
}

impl RpcClientImplFactory {
    pub fn new(grpc_config: RpcConfig, rpc_opts: RpcOptions) -> Self {
        Self {
            rpc_opts,
            grpc_config,
        }
    }

    #[inline]
    fn make_endpoint_with_scheme(endpoint: &str) -> String {
        format!("http://{}", endpoint)
    }
}

#[async_trait]
impl RpcClientFactory for RpcClientImplFactory {
    /// The endpoint should be in the form: `{ip_addr}:{port}`.
    async fn build(&self, endpoint: String) -> Result<Arc<dyn RpcClient>> {
        let endpoint_with_scheme = Self::make_endpoint_with_scheme(&endpoint);
        let configured_endpoint =
            Endpoint::from_shared(endpoint_with_scheme).map_err(|e| Error::Connect {
                addr: endpoint.clone(),
                source: Box::new(e),
            })?;

        let configured_endpoint = match self.grpc_config.keep_alive_while_idle {
            true => configured_endpoint
                .connect_timeout(self.rpc_opts.connect_timeout)
                .keep_alive_timeout(self.grpc_config.keep_alive_timeout)
                .keep_alive_while_idle(true)
                .http2_keep_alive_interval(self.grpc_config.keep_alive_interval),
            false => configured_endpoint
                .connect_timeout(self.rpc_opts.connect_timeout)
                .keep_alive_while_idle(false),
        };
        let channel = configured_endpoint
            .connect()
            .await
            .map_err(|e| Error::Connect {
                addr: endpoint,
                source: Box::new(e),
            })?;
        Ok(Arc::new(RpcClientImpl::new(
            channel,
            self.rpc_opts.read_timeout,
            self.rpc_opts.write_timeout,
        )))
    }
}
