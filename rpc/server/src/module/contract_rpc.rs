// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::module::helpers::TransactionRequestFiller;
use crate::module::map_err;
use anyhow::format_err;
use futures::future::TryFutureExt;
use futures::FutureExt;
use starcoin_abi_decoder::{decode_move_value, DecodedMoveValue};
use starcoin_abi_resolver::ABIResolver;
use starcoin_abi_types::{ModuleABI, ScriptFunctionABI, StructABI, TypeABI};
use starcoin_account_api::AccountAsyncService;
use starcoin_config::NodeConfig;
use starcoin_dev::playground::{call_contract, PlaygroudService};
use starcoin_rpc_api::contract_api::ContractApi;
use starcoin_rpc_api::types::{
    AnnotatedMoveStructView, AnnotatedMoveValueView, ContractCall, DryRunOutputView,
    DryRunTransactionRequest, FunctionIdView, ModuleIdView, StrView, StructTagView,
    TransactionOutputView, WriteOpValueView,
};
use starcoin_rpc_api::FutureResult;
use starcoin_state_api::ChainStateAsyncService;
use starcoin_statedb::ChainStateDB;
use starcoin_storage::Storage;
use starcoin_txpool_api::TxPoolSyncService;
use starcoin_types::account_address::AccountAddress;
use starcoin_types::language_storage::{ModuleId, StructTag};
use starcoin_types::transaction::{DryRunTransaction, RawUserTransaction};
use starcoin_vm_types::access_path::AccessPath;
use starcoin_vm_types::state_view::StateView;
use starcoin_vm_types::transaction::authenticator::AccountPublicKey;
use starcoin_vm_types::transaction::TransactionArgument;
use std::str::FromStr;
use std::sync::Arc;

pub struct ContractRpcImpl<Account, Pool, State> {
    pub(crate) account: Option<Account>,
    pub(crate) pool: Pool,
    pub(crate) chain_state: State,
    pub(crate) node_config: Arc<NodeConfig>,
    playground: PlaygroudService,
    storage: Arc<Storage>,
}

impl<Account, Pool, State> ContractRpcImpl<Account, Pool, State>
where
    Account: AccountAsyncService + 'static,
    Pool: TxPoolSyncService + 'static,
    State: ChainStateAsyncService + 'static,
{
    pub fn new(
        node_config: Arc<NodeConfig>,
        account: Option<Account>,
        pool: Pool,
        chain_state: State,
        playground: PlaygroudService,
        storage: Arc<Storage>,
    ) -> Self {
        Self {
            account,
            pool,
            chain_state,
            node_config,
            playground,
            storage,
        }
    }
    fn txn_request_filler(&self) -> TransactionRequestFiller<Account, Pool, State> {
        TransactionRequestFiller {
            account: self.account.clone(),
            pool: self.pool.clone(),
            chain_state: self.chain_state.clone(),
            node_config: self.node_config.clone(),
        }
    }
}

impl<Account, Pool, State> ContractApi for ContractRpcImpl<Account, Pool, State>
where
    Account: AccountAsyncService + 'static,
    Pool: TxPoolSyncService + 'static,
    State: ChainStateAsyncService + 'static,
{
    fn get_code(&self, module_id: StrView<ModuleId>) -> FutureResult<Option<StrView<Vec<u8>>>> {
        let service = self.chain_state.clone();
        let f = async move {
            let code = service.get(AccessPath::from(&module_id.0)).await?;
            Ok(code.map(StrView))
        };
        Box::pin(f.map_err(map_err).boxed())
    }

    fn get_resource(
        &self,
        addr: AccountAddress,
        resource_type: StrView<StructTag>,
    ) -> FutureResult<Option<AnnotatedMoveStructView>> {
        let service = self.chain_state.clone();
        let playground = self.playground.clone();
        let f = async move {
            let state_root = service.clone().state_root().await?;
            let data = service
                .get(AccessPath::resource_access_path(
                    addr,
                    resource_type.0.clone(),
                ))
                .await?;
            match data {
                None => Ok(None),
                Some(d) => {
                    let value =
                        playground.view_resource(state_root, &resource_type.0, d.as_slice())?;
                    Ok(Some(value.into()))
                }
            }
        };
        Box::pin(f.map_err(map_err).boxed())
    }
    fn call(&self, call: ContractCall) -> FutureResult<Vec<AnnotatedMoveValueView>> {
        let service = self.chain_state.clone();
        let playground = self.playground.clone();
        let ContractCall {
            function_id,
            type_args,
            args,
        } = call;
        let f = async move {
            let state_root = service.state_root().await?;
            let output = playground.call_contract(
                state_root,
                function_id.0.module,
                function_id.0.function,
                type_args.into_iter().map(|v| v.0).collect(),
                args.into_iter().map(|v| v.0).collect(),
            )?;
            Ok(output.into_iter().map(Into::into).collect())
        }
        .map_err(map_err);
        Box::pin(f.boxed())
    }

    fn call_v2(&self, call: ContractCall) -> FutureResult<Vec<DecodedMoveValue>> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let ContractCall {
            function_id,
            type_args,
            args,
        } = call;

        let f = async move {
            let state_root = service.state_root().await?;
            let state = ChainStateDB::new(storage, Some(state_root));

            // check arg types.
            {
                let func_abi = ABIResolver::new(&state).resolve_function(
                    &function_id.0.module,
                    function_id.0.function.as_ident_str(),
                )?;
                anyhow::ensure!(
                    func_abi.ty_args().len() == type_args.len(),
                    "type args length mismatch, expect {}, actual {}",
                    func_abi.ty_args().len(),
                    type_args.len()
                );

                let arg_abi = func_abi.args();
                anyhow::ensure!(
                    arg_abi.len() == args.len(),
                    "args length mismatch, expect {}, actual {}",
                    arg_abi.len(),
                    args.len()
                );
                for (i, (abi, v)) in arg_abi.iter().zip(&args).enumerate() {
                    match (abi.type_abi(), &v.0) {
                        (TypeABI::U8, TransactionArgument::U8(_))
                        | (TypeABI::U64, TransactionArgument::U64(_))
                        | (TypeABI::U128, TransactionArgument::U128(_))
                        | (TypeABI::Address, TransactionArgument::Address(_))
                        | (TypeABI::Bool, TransactionArgument::Bool(_)) => {}
                        (TypeABI::Vector(sub_ty), TransactionArgument::U8Vector(_))
                            if sub_ty.as_ref() == &TypeABI::U8 => {}
                        (abi, value) => anyhow::bail!(
                            "arg type at position {} mismatch, expect {:?}, actual {}",
                            i,
                            abi,
                            value
                        ),
                    }
                }
            }
            let output = call_contract(
                &state,
                function_id.0.module,
                function_id.0.function.as_str(),
                type_args.into_iter().map(|v| v.0).collect(),
                args.into_iter().map(|v| v.0).collect(),
            )?;
            Ok(output.into_iter().map(Into::into).collect())
        }
        .map_err(map_err);
        Box::pin(f.boxed())
    }

    fn dry_run(&self, txn: DryRunTransactionRequest) -> FutureResult<DryRunOutputView> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let txn_builder = self.txn_request_filler();
        let f = async move {
            let state_root = service.state_root().await?;
            let DryRunTransactionRequest {
                transaction,
                sender_public_key,
            } = txn;

            let txn = txn_builder.fill_transaction(transaction).await?;
            let state_view = ChainStateDB::new(storage, Some(state_root));
            dry_run(
                &state_view,
                DryRunTransaction {
                    raw_txn: txn,
                    public_key: sender_public_key.0,
                },
            )
        }
        .map_err(map_err);
        Box::pin(f.boxed())
    }

    fn dry_run_raw(
        &self,
        raw_txn: String,
        sender_public_key: StrView<AccountPublicKey>,
    ) -> FutureResult<DryRunOutputView> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let f = async move {
            let state_root = service.state_root().await?;
            let raw_txn = RawUserTransaction::from_str(raw_txn.as_str())?;
            let state_view = ChainStateDB::new(storage, Some(state_root));
            dry_run(
                &state_view,
                DryRunTransaction {
                    raw_txn,
                    public_key: sender_public_key.0,
                },
            )
        }
        .map_err(map_err);
        Box::pin(f.boxed())
    }

    fn resolve_function(&self, function_id: FunctionIdView) -> FutureResult<ScriptFunctionABI> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let fut = async move {
            let state = ChainStateDB::new(storage, Some(service.state_root().await?));
            ABIResolver::new(&state)
                .resolve_function(&function_id.0.module, function_id.0.function.as_ident_str())
        }
        .map_err(map_err);
        Box::pin(fut.boxed())
    }

    fn resolve_struct(&self, struct_tag: StructTagView) -> FutureResult<StructABI> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let fut = async move {
            let state = ChainStateDB::new(storage, Some(service.state_root().await?));
            ABIResolver::new(&state).resolve_struct_tag(&struct_tag.0)
        }
        .map_err(map_err);
        Box::pin(fut.boxed())
    }

    fn resolve_module(&self, module_id: ModuleIdView) -> FutureResult<ModuleABI> {
        let service = self.chain_state.clone();
        let storage = self.storage.clone();
        let fut = async move {
            let state = ChainStateDB::new(storage, Some(service.state_root().await?));
            ABIResolver::new(&state).resolve_module(&module_id.0)
        }
        .map_err(map_err);
        Box::pin(fut.boxed())
    }
}

pub fn dry_run(
    state_view: &dyn StateView,
    txn: DryRunTransaction,
) -> anyhow::Result<DryRunOutputView> {
    let (vm_status, output) = starcoin_dev::playground::dry_run(state_view, txn)?;
    let vm_status_explain = vm_status_translator::explain_vm_status(state_view, vm_status)?;
    let mut txn_output: TransactionOutputView = output.into();
    let resolver = ABIResolver::new(state_view);
    for action in txn_output.write_set.iter_mut() {
        let access_path = action.access_path.clone();
        if let Some(value) = &mut action.value {
            match value {
                WriteOpValueView::Code(view) => {
                    view.abi = Some(resolver.resolve_module_code(view.code.0.as_slice())?);
                }
                WriteOpValueView::Resource(view) => {
                    let struct_tag = access_path.path.as_struct_tag().ok_or_else(|| {
                        format_err!("invalid resource access path: {}", access_path)
                    })?;
                    let struct_abi = resolver.resolve_struct_tag(struct_tag)?;
                    view.json = Some(decode_move_value(
                        &TypeABI::Struct(Box::new(struct_abi)),
                        view.raw.0.as_slice(),
                    )?)
                }
            }
        }
    }
    Ok(DryRunOutputView {
        explained_status: vm_status_explain,
        txn_output,
    })
}
