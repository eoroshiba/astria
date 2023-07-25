use std::{
    collections::VecDeque,
    pin::Pin,
    task::{
        Context,
        Poll,
    },
};

use anyhow::{
    bail,
    Context as _,
};
use borsh::BorshSerialize as _;
use futures::{
    Future,
    FutureExt,
};
use penumbra_storage::Storage;
use penumbra_tower_trace::RequestExt as _;
use tendermint::{
    abci::{
        request,
        response::{
            self,
            Echo,
            SetOption,
        },
        Code,
        InfoRequest,
        InfoResponse,
    },
    block::Height,
    AppHash,
};
use tower::Service;
use tower_abci::BoxError;
use tracing::{
    debug,
    instrument,
    warn,
    Instrument,
};

use crate::{
    accounts::query::QueryHandler,
    state_ext::StateReadExt,
};

#[derive(Clone)]
pub(crate) struct Info {
    storage: Storage,
}

impl Info {
    pub(crate) fn new(storage: Storage) -> Self {
        Self {
            storage,
        }
    }
}

impl Service<InfoRequest> for Info {
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = InfoResponse;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: InfoRequest) -> Self::Future {
        let span = req.create_span();

        handle_info_request(self.storage.clone(), req)
            .instrument(span)
            .boxed()
    }
}

#[instrument(skip(storage))]
async fn handle_info_request(
    storage: Storage,
    request: InfoRequest,
) -> Result<InfoResponse, BoxError> {
    match &request {
        InfoRequest::Info(_) => {
            let response = InfoResponse::Info(response::Info {
                version: "0.1.0".to_string(),
                app_version: 1,
                last_block_height: Height::default(),
                last_block_app_hash: AppHash::default(),
                data: "astria_sequencer".to_string(),
            });
            Ok(response)
        }
        InfoRequest::Echo(echo) => Ok(InfoResponse::Echo(Echo {
            message: echo.message.clone(),
        })),
        InfoRequest::Query(req) => Ok(InfoResponse::Query(
            handle_query(storage, req)
                .await
                .context("failed handling query request")?,
        )),
        // this was removed after v0.34
        InfoRequest::SetOption(_) => Ok(InfoResponse::SetOption(SetOption {
            code: Code::default(),
            log: "SetOption is not supported".to_string(),
            info: "SetOption is not supported".to_string(),
        })),
    }
}

/// handles queries in the form of [`component/arg1/arg2/...`]
/// for example, to query an account balance: [`accounts/balance/0x1234...`]
async fn handle_query(
    storage: Storage,
    request: &request::Query,
) -> anyhow::Result<response::Query> {
    // note: request::Query also has a `data` field, which we ignore here
    let query = decode_query(&request.path).context("failed to decode query")?;

    debug!("handling query");
    let state = match request.height.value() {
        0 => storage.latest_snapshot(),
        height => {
            let version = storage
                .latest_snapshot()
                .get_storage_version_by_height(height)
                .await
                .context("failed to get storage version from height")?;
            storage
                .snapshot(version)
                .context("failed to get storage at version")?
        }
    };

    let height: Height = state
        .get_block_height()
        .await
        .context("failed to get block height")?
        .try_into()
        .context("internal u64 block height does not fit into tendermint i64 `Height`")?;

    let key = request.path.clone().into_bytes();
    let value = match query {
        Query::AccountsQuery(request) => {
            let handler = QueryHandler::new();
            handler
                .handle(state, request)
                .await
                .context("failed to handle accounts query")?
        }
    }
    .try_to_vec()
    .context("failed serializing query response")?;

    Ok(response::Query {
        key: key.into(),
        value: value.into(),
        height,
        ..Default::default()
    })
}

#[non_exhaustive]
pub(crate) enum Query {
    AccountsQuery(crate::accounts::query::Request),
}

#[instrument]
fn decode_query(path: &str) -> anyhow::Result<Query> {
    let mut parts: VecDeque<&str> = path.split('/').collect();

    let Some(component) = parts.pop_front() else {
        bail!("invalid query path; missing component: {path}");
    };

    match component {
        "accounts" => {
            let request = crate::accounts::query::Request::decode(parts)
                .context("failed to decode accounts query from path parts")?;
            Ok(Query::AccountsQuery(request))
        }
        other => bail!("unknown query path: `{other}`"),
    }
}

#[cfg(test)]
mod test {
    use penumbra_storage::StateDelta;
    use tendermint::abci::request;

    use super::*;
    use crate::{
        accounts::{
            state_ext::StateWriteExt as _,
            types::Address,
        },
        state_ext::StateWriteExt as _,
    };

    #[tokio::test]
    async fn test_handle_query() {
        let storage = penumbra_storage::TempStorage::new()
            .await
            .expect("failed to create temp storage backing chain state");

        let height = 99;
        let version = storage.latest_version().wrapping_add(1);
        let mut state = StateDelta::new(storage.latest_snapshot());
        state.put_storage_version_by_height(height, version);

        let address = Address::try_from_str("a034c743bed8f26cb8ee7b8db2230fd8347ae131").unwrap();
        state.put_account_balance(&address, 1000.into()).unwrap();
        state.put_block_height(height);

        storage.commit(state).await.unwrap();

        let query = request::Query {
            path: "accounts/balance/a034c743bed8f26cb8ee7b8db2230fd8347ae131".to_string(),
            data: vec![].into(),
            height: u32::try_from(height).unwrap().into(),
            prove: false,
        };
        handle_query(storage.clone(), &query).await.unwrap();
    }
}
