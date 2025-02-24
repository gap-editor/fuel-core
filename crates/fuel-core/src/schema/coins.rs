use std::collections::HashSet;

use crate::{
    coins_query::{
        random_improve,
        select_coins_to_spend,
        CoinsQueryError,
        ExcludedCoinIds,
        SpendQuery,
    },
    database::database_description::IndexationKind,
    fuel_core_graphql_api::{
        query_costs,
        storage::coins::CoinsToSpendIndexKey,
        IntoApiResult,
    },
    graphql_api::{
        api_service::ConsensusProvider,
        database::ReadView,
    },
    query::asset_query::AssetSpendTarget,
    schema::{
        scalars::{
            Address,
            AssetId,
            Nonce,
            UtxoId,
            U16,
            U32,
            U64,
        },
        ReadViewProvider,
    },
};
use async_graphql::{
    connection::{
        Connection,
        EmptyFields,
    },
    Context,
};
use fuel_core_types::{
    entities::coins::{
        self,
        coin::Coin as CoinModel,
        message_coin::{
            self,
            MessageCoin as MessageCoinModel,
        },
        CoinId,
    },
    fuel_tx::{
        self,
    },
};
use itertools::Itertools;
use tokio_stream::StreamExt;

pub struct Coin(pub(crate) CoinModel);

#[async_graphql::Object]
impl Coin {
    async fn utxo_id(&self) -> UtxoId {
        self.0.utxo_id.into()
    }

    async fn owner(&self) -> Address {
        self.0.owner.into()
    }

    async fn amount(&self) -> U64 {
        self.0.amount.into()
    }

    async fn asset_id(&self) -> AssetId {
        self.0.asset_id.into()
    }

    /// TxPointer - the height of the block this coin was created in
    async fn block_created(&self) -> U32 {
        u32::from(self.0.tx_pointer.block_height()).into()
    }

    /// TxPointer - the index of the transaction that created this coin
    async fn tx_created_idx(&self) -> U16 {
        self.0.tx_pointer.tx_index().into()
    }
}

impl From<CoinModel> for Coin {
    fn from(value: CoinModel) -> Self {
        Coin(value)
    }
}

pub struct MessageCoin(pub(crate) MessageCoinModel);

#[async_graphql::Object]
impl MessageCoin {
    async fn sender(&self) -> Address {
        self.0.sender.into()
    }

    async fn recipient(&self) -> Address {
        self.0.recipient.into()
    }

    async fn nonce(&self) -> Nonce {
        self.0.nonce.into()
    }

    async fn amount(&self) -> U64 {
        self.0.amount.into()
    }

    #[graphql(complexity = "query_costs().storage_read")]
    async fn asset_id(&self, ctx: &Context<'_>) -> AssetId {
        let params = ctx
            .data_unchecked::<ConsensusProvider>()
            .current_consensus_params();

        let base_asset_id = *params.base_asset_id();
        base_asset_id.into()
    }

    async fn da_height(&self) -> U64 {
        self.0.da_height.0.into()
    }
}

impl From<MessageCoinModel> for MessageCoin {
    fn from(value: MessageCoinModel) -> Self {
        MessageCoin(value)
    }
}

/// The schema analog of the [`coins::CoinType`].
#[derive(async_graphql::Union)]
pub enum CoinType {
    /// The regular coins generated by the transaction output.
    Coin(Coin),
    /// The bridged coin from the DA layer.
    MessageCoin(MessageCoin),
}

impl From<coins::CoinType> for CoinType {
    fn from(value: coins::CoinType) -> Self {
        match value {
            coins::CoinType::Coin(coin) => CoinType::Coin(coin.into()),
            coins::CoinType::MessageCoin(coin) => CoinType::MessageCoin(coin.into()),
        }
    }
}

#[derive(async_graphql::InputObject)]
struct CoinFilterInput {
    /// Returns coins owned by the `owner`.
    owner: Address,
    /// Returns coins only with `asset_id`.
    asset_id: Option<AssetId>,
}

#[derive(async_graphql::InputObject)]
pub struct SpendQueryElementInput {
    /// Identifier of the asset to spend.
    asset_id: AssetId,
    /// Target amount for the query.
    amount: U64,
    /// The maximum number of currencies for selection.
    max: Option<U32>,
}

#[derive(async_graphql::InputObject)]
pub struct ExcludeInput {
    /// Utxos to exclude from the selection.
    utxos: Vec<UtxoId>,
    /// Messages to exclude from the selection.
    messages: Vec<Nonce>,
}

#[derive(Default)]
pub struct CoinQuery;

#[async_graphql::Object]
impl CoinQuery {
    /// Gets the coin by `utxo_id`.
    #[graphql(complexity = "query_costs().storage_read + child_complexity")]
    async fn coin(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The ID of the coin")] utxo_id: UtxoId,
    ) -> async_graphql::Result<Option<Coin>> {
        let query = ctx.read_view()?;
        query.coin(utxo_id.0).into_api_result()
    }

    /// Gets all unspent coins of some `owner` maybe filtered with by `asset_id` per page.
    #[graphql(complexity = "{\
        query_costs().storage_iterator\
        + (query_costs().storage_read + first.unwrap_or_default() as usize) * child_complexity \
        + (query_costs().storage_read + last.unwrap_or_default() as usize) * child_complexity\
    }")]
    async fn coins(
        &self,
        ctx: &Context<'_>,
        filter: CoinFilterInput,
        first: Option<i32>,
        after: Option<String>,
        last: Option<i32>,
        before: Option<String>,
    ) -> async_graphql::Result<Connection<UtxoId, Coin, EmptyFields, EmptyFields>> {
        let query = ctx.read_view()?;
        let owner: fuel_tx::Address = filter.owner.into();
        crate::schema::query_pagination(after, before, first, last, |start, direction| {
            let coins = query
                .owned_coins(&owner, (*start).map(Into::into), direction)
                .filter_map(|result| {
                    if let (Ok(coin), Some(filter_asset_id)) = (&result, &filter.asset_id)
                    {
                        if coin.asset_id != filter_asset_id.0 {
                            return None
                        }
                    }

                    Some(result)
                })
                .map(|res| res.map(|coin| (coin.utxo_id.into(), coin.into())));

            Ok(coins)
        })
        .await
    }

    /// For each `query_per_asset`, get some spendable coins(of asset specified by the query) owned by
    /// `owner` that add up at least the query amount. The returned coins can be spent.
    /// The number of coins is optimized to prevent dust accumulation.
    ///
    /// The query supports excluding and maximum the number of coins.
    ///
    /// Returns:
    ///     The list of spendable coins per asset from the query. The length of the result is
    ///     the same as the length of `query_per_asset`. The ordering of assets and `query_per_asset`
    ///     is the same.
    #[graphql(complexity = "query_costs().coins_to_spend")]
    async fn coins_to_spend(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The `Address` of the coins owner.")] owner: Address,
        #[graphql(desc = "\
            The list of requested assets` coins with asset ids, `target` amount the user wants \
            to reach, and the `max` number of coins in the selection. Several entries with the \
            same asset id are not allowed. The result can't contain more coins than `max_inputs`.")]
        mut query_per_asset: Vec<SpendQueryElementInput>,
        #[graphql(desc = "The excluded coins from the selection.")] excluded_ids: Option<
            ExcludeInput,
        >,
    ) -> async_graphql::Result<Vec<Vec<CoinType>>> {
        let params = ctx
            .data_unchecked::<ConsensusProvider>()
            .current_consensus_params();
        let max_input = params.tx_params().max_inputs();

        let excluded_id_count = excluded_ids.as_ref().map_or(0, |exclude| {
            exclude.utxos.len().saturating_add(exclude.messages.len())
        });
        if excluded_id_count > max_input as usize {
            return Err(CoinsQueryError::TooManyExcludedId {
                provided: excluded_id_count,
                allowed: max_input,
            }
            .into());
        }

        let mut duplicate_checker = HashSet::with_capacity(query_per_asset.len());
        for query in &query_per_asset {
            let asset_id: fuel_tx::AssetId = query.asset_id.into();
            if !duplicate_checker.insert(asset_id) {
                return Err(CoinsQueryError::DuplicateAssets(asset_id).into());
            }
        }

        let owner: fuel_tx::Address = owner.0;

        // `coins_to_spend` exists to help select inputs for the transactions.
        // It doesn't make sense to allow the user to request more than the maximum number
        // of inputs.
        // TODO: To avoid breaking changes, we will truncate request for now.
        //  In the future, we should return an error if the input is too large.
        //  https://github.com/FuelLabs/fuel-core/issues/2343
        query_per_asset.truncate(max_input as usize);

        let read_view = ctx.read_view()?;
        let indexation_available = read_view
            .indexation_flags
            .contains(&IndexationKind::CoinsToSpend);
        if indexation_available {
            coins_to_spend_with_cache(
                owner,
                query_per_asset,
                excluded_ids,
                max_input,
                read_view.as_ref(),
            )
            .await
        } else {
            let base_asset_id = params.base_asset_id();
            coins_to_spend_without_cache(
                owner,
                query_per_asset,
                excluded_ids,
                max_input,
                base_asset_id,
                read_view.as_ref(),
            )
            .await
        }
    }
}

async fn coins_to_spend_without_cache(
    owner: fuel_tx::Address,
    query_per_asset: Vec<SpendQueryElementInput>,
    excluded_ids: Option<ExcludeInput>,
    max_input: u16,
    base_asset_id: &fuel_tx::AssetId,
    db: &ReadView,
) -> async_graphql::Result<Vec<Vec<CoinType>>> {
    let query_per_asset = query_per_asset
        .into_iter()
        .map(|e| {
            AssetSpendTarget::new(
                e.asset_id.0,
                e.amount.0,
                e.max
                    .and_then(|max| u16::try_from(max.0).ok())
                    .unwrap_or(max_input)
                    .min(max_input),
            )
        })
        .collect_vec();
    let excluded_ids: Option<Vec<_>> = excluded_ids.map(|exclude| {
        let utxos = exclude
            .utxos
            .into_iter()
            .map(|utxo| coins::CoinId::Utxo(utxo.into()));
        let messages = exclude
            .messages
            .into_iter()
            .map(|message| coins::CoinId::Message(message.into()));
        utxos.chain(messages).collect()
    });

    let spend_query =
        SpendQuery::new(owner, &query_per_asset, excluded_ids, *base_asset_id)?;

    let all_coins = random_improve(db, &spend_query)
        .await?
        .into_iter()
        .map(|coins| {
            coins
                .into_iter()
                .map(|coin| match coin {
                    coins::CoinType::Coin(coin) => CoinType::Coin(coin.into()),
                    coins::CoinType::MessageCoin(coin) => {
                        CoinType::MessageCoin(coin.into())
                    }
                })
                .collect_vec()
        })
        .collect();

    Ok(all_coins)
}

async fn coins_to_spend_with_cache(
    owner: fuel_tx::Address,
    query_per_asset: Vec<SpendQueryElementInput>,
    excluded_ids: Option<ExcludeInput>,
    max_input: u16,
    db: &ReadView,
) -> async_graphql::Result<Vec<Vec<CoinType>>> {
    let mut all_coins = Vec::with_capacity(query_per_asset.len());

    let excluded = ExcludedCoinIds::new(
        excluded_ids
            .iter()
            .flat_map(|exclude| exclude.utxos.iter())
            .map(|utxo_id| &utxo_id.0),
        excluded_ids
            .iter()
            .flat_map(|exclude| exclude.messages.iter())
            .map(|nonce| &nonce.0),
    );

    for asset in query_per_asset {
        let asset_id = asset.asset_id.0;
        let total_amount = asset.amount.0;
        let max = asset
            .max
            .and_then(|max| u16::try_from(max.0).ok())
            .unwrap_or(max_input)
            .min(max_input);

        let selected_coins = select_coins_to_spend(
            db.off_chain.coins_to_spend_index(&owner, &asset_id),
            total_amount,
            max,
            &asset_id,
            &excluded,
            db.batch_size,
        )
        .await?;

        let mut coins_per_asset = Vec::with_capacity(selected_coins.len());
        for coin_or_message_id in into_coin_id(&selected_coins) {
            let coin_type = match coin_or_message_id {
                coins::CoinId::Utxo(utxo_id) => {
                    db.coin(utxo_id).map(|coin| CoinType::Coin(coin.into()))?
                }
                coins::CoinId::Message(nonce) => {
                    let message = db.message(&nonce)?;
                    let message_coin: message_coin::MessageCoin = message.try_into()?;
                    CoinType::MessageCoin(message_coin.into())
                }
            };

            coins_per_asset.push(coin_type);
        }

        all_coins.push(coins_per_asset);
    }
    Ok(all_coins)
}

fn into_coin_id(selected: &[CoinsToSpendIndexKey]) -> Vec<CoinId> {
    let mut coins = Vec::with_capacity(selected.len());
    for coin in selected {
        let coin = match coin {
            CoinsToSpendIndexKey::Coin { utxo_id, .. } => CoinId::Utxo(*utxo_id),
            CoinsToSpendIndexKey::Message { nonce, .. } => CoinId::Message(*nonce),
        };
        coins.push(coin);
    }
    coins
}
