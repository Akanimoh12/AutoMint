#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror, symbol_short,
    token::Client as TokenClient,
    Address, Env, IntoVal, Symbol, Val, Vec,
};

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Listing(u64),
    ActiveListings,
    UserListings(Address),
    UserPurchases(Address),
    NextListingId,
    Config,
    Initialized,
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct Listing {
    pub id: u64,
    pub seller: Address,
    pub bot_id: u64,
    pub bot_tier: u32,
    pub price: i128,
    pub currency: Address,
    pub listed_at: u64,
    pub active: bool,
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct Purchase {
    pub listing_id: u64,
    pub bot_id: u64,
    pub seller: Address,
    pub price: i128,
    pub currency: Address,
    pub purchased_at: u64,
}

#[derive(Clone)]
#[contracttype]
pub struct Config {
    pub bot_nft: Address,
    pub admin: Address,
    pub fee_bps: u32,
    pub fee_recipient: Address,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum MarketplaceError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    ListingNotFound = 4,
    ListingInactive = 5,
    SelfPurchase = 6,
    BotTransferFailed = 7,
    PaymentFailed = 8,
    InvalidPrice = 9,
}

const LEDGER_BUMP: u32 = 120960;
const LEDGER_THRESHOLD: u32 = 103680;
const FEE_BPS: u32 = 250;

fn call_bot_transfer(env: &Env, bot_nft: &Address, from: &Address, to: &Address, bot_id: u64) {
    let args: Vec<Val> = soroban_sdk::vec![
        env,
        from.into_val(env),
        to.into_val(env),
        bot_id.into_val(env)
    ];
    let _: () = env.invoke_contract(bot_nft, &Symbol::new(env, "transfer"), args);
}

#[contract]
pub struct MarketplaceContract;

#[contractimpl]
impl MarketplaceContract {
    pub fn initialize(
        env: Env,
        admin: Address,
        bot_nft: Address,
        fee_recipient: Address,
    ) -> Result<(), MarketplaceError> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(MarketplaceError::AlreadyInitialized);
        }
        admin.require_auth();
        let config = Config { bot_nft, admin, fee_bps: FEE_BPS, fee_recipient };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::NextListingId, &1u64);
        env.storage().instance().set(&DataKey::ActiveListings, &Vec::<u64>::new(&env));
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        Ok(())
    }

    pub fn list_bot(
        env: Env,
        seller: Address,
        bot_id: u64,
        bot_tier: u32,
        price: i128,
        currency: Address,
    ) -> Result<u64, MarketplaceError> {
        seller.require_auth();
        if price <= 0 {
            return Err(MarketplaceError::InvalidPrice);
        }
        let config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(MarketplaceError::NotInitialized)?;
        let marketplace = env.current_contract_address();
        call_bot_transfer(&env, &config.bot_nft, &seller, &marketplace, bot_id);
        let listing_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextListingId)
            .unwrap_or(1);
        let listing = Listing {
            id: listing_id,
            seller: seller.clone(),
            bot_id,
            bot_tier,
            price,
            currency,
            listed_at: env.ledger().timestamp(),
            active: true,
        };
        env.storage().persistent().set(&DataKey::Listing(listing_id), &listing);
        env.storage().persistent().extend_ttl(
            &DataKey::Listing(listing_id),
            LEDGER_THRESHOLD,
            LEDGER_BUMP,
        );
        let mut active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveListings)
            .unwrap_or_else(|| Vec::new(&env));
        active.push_back(listing_id);
        env.storage().instance().set(&DataKey::ActiveListings, &active);
        let mut user_listings: Vec<u64> = env
            .storage()
            .persistent()
            .get::<_, Vec<u64>>(&DataKey::UserListings(seller.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        user_listings.push_back(listing_id);
        env.storage().persistent().set(&DataKey::UserListings(seller.clone()), &user_listings);
        env.storage().instance().set(&DataKey::NextListingId, &(listing_id + 1));
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            (symbol_short!("listed"), seller, listing_id),
            (bot_id, price),
        );
        Ok(listing_id)
    }

    pub fn buy_bot(env: Env, buyer: Address, listing_id: u64) -> Result<(), MarketplaceError> {
        buyer.require_auth();
        let mut listing: Listing = env
            .storage()
            .persistent()
            .get(&DataKey::Listing(listing_id))
            .ok_or(MarketplaceError::ListingNotFound)?;
        if !listing.active {
            return Err(MarketplaceError::ListingInactive);
        }
        if listing.seller == buyer {
            return Err(MarketplaceError::SelfPurchase);
        }
        let config: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        let marketplace = env.current_contract_address();
        let fee = listing.price * (config.fee_bps as i128) / 10_000;
        let seller_amount = listing.price - fee;
        let token = TokenClient::new(&env, &listing.currency);
        token.transfer(&buyer, &listing.seller, &seller_amount);
        if fee > 0 {
            token.transfer(&buyer, &config.fee_recipient, &fee);
        }
        call_bot_transfer(&env, &config.bot_nft, &marketplace, &buyer, listing.bot_id);
        listing.active = false;
        env.storage().persistent().set(&DataKey::Listing(listing_id), &listing);
        Self::remove_active_listing(&env, listing_id);
        let purchase = Purchase {
            listing_id,
            bot_id: listing.bot_id,
            seller: listing.seller.clone(),
            price: listing.price,
            currency: listing.currency,
            purchased_at: env.ledger().timestamp(),
        };
        Self::add_user_purchase(&env, &buyer, purchase);
        env.events().publish(
            (symbol_short!("sold"), listing.seller.clone(), buyer.clone()),
            (listing_id, listing.bot_id, listing.price),
        );
        Ok(())
    }

    pub fn cancel_listing(
        env: Env,
        seller: Address,
        listing_id: u64,
    ) -> Result<(), MarketplaceError> {
        seller.require_auth();
        let mut listing: Listing = env
            .storage()
            .persistent()
            .get(&DataKey::Listing(listing_id))
            .ok_or(MarketplaceError::ListingNotFound)?;
        if !listing.active {
            return Err(MarketplaceError::ListingInactive);
        }
        if listing.seller != seller {
            return Err(MarketplaceError::Unauthorized);
        }
        let config: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        let marketplace = env.current_contract_address();
        call_bot_transfer(&env, &config.bot_nft, &marketplace, &seller, listing.bot_id);
        listing.active = false;
        env.storage().persistent().set(&DataKey::Listing(listing_id), &listing);
        Self::remove_active_listing(&env, listing_id);
        env.events().publish(
            (symbol_short!("cancel"), seller, listing_id),
            listing.bot_id,
        );
        Ok(())
    }

    pub fn get_listing(env: Env, listing_id: u64) -> Result<Listing, MarketplaceError> {
        env.storage()
            .persistent()
            .get(&DataKey::Listing(listing_id))
            .ok_or(MarketplaceError::ListingNotFound)
    }

    pub fn get_active_listings(env: Env, start: u32, limit: u32) -> Vec<Listing> {
        let active_ids: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveListings)
            .unwrap_or_else(|| Vec::new(&env));
        let mut result: Vec<Listing> = Vec::new(&env);
        let mut count = 0u32;
        for (i, id) in active_ids.iter().enumerate() {
            if (i as u32) < start { continue; }
            if count >= limit { break; }
            if let Some(l) = env.storage().persistent().get::<_, Listing>(&DataKey::Listing(id)) {
                if l.active {
                    result.push_back(l);
                    count += 1;
                }
            }
        }
        result
    }

    pub fn get_user_listings(env: Env, seller: Address) -> Vec<Listing> {
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get::<_, Vec<u64>>(&DataKey::UserListings(seller))
            .unwrap_or_else(|| Vec::new(&env));
        let mut result: Vec<Listing> = Vec::new(&env);
        for id in ids.iter() {
            if let Some(l) = env.storage().persistent().get::<_, Listing>(&DataKey::Listing(id)) {
                result.push_back(l);
            }
        }
        result
    }

    pub fn get_user_purchases(env: Env, buyer: Address, limit: u32) -> Vec<Purchase> {
        let purchases: Vec<Purchase> = env
            .storage()
            .persistent()
            .get::<_, Vec<Purchase>>(&DataKey::UserPurchases(buyer))
            .unwrap_or_else(|| Vec::new(&env));
        let mut result: Vec<Purchase> = Vec::new(&env);
        let start = if purchases.len() > limit as usize {
            purchases.len() - (limit as usize)
        } else {
            0
        };
        for i in start..purchases.len() {
            result.push_back(purchases.get(i as u32).unwrap().clone());
        }
        result
    }

    pub fn config(env: Env) -> Config {
        env.storage().instance().get(&DataKey::Config).unwrap()
    }

    fn remove_active_listing(env: &Env, listing_id: u64) {
        let active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveListings)
            .unwrap_or_else(|| Vec::new(env));
        let mut new_active: Vec<u64> = Vec::new(env);
        for id in active.iter() {
            if id != listing_id {
                new_active.push_back(id);
            }
        }
        env.storage().instance().set(&DataKey::ActiveListings, &new_active);
    }

    fn add_user_purchase(env: &Env, buyer: &Address, purchase: Purchase) {
        let mut purchases: Vec<Purchase> = env
            .storage()
            .persistent()
            .get::<_, Vec<Purchase>>(&DataKey::UserPurchases(buyer.clone()))
            .unwrap_or_else(|| Vec::new(env));
        purchases.push_back(purchase);
        env.storage().persistent().set(&DataKey::UserPurchases(buyer.clone()), &purchases);
        env.storage().persistent().extend_ttl(
            &DataKey::UserPurchases(buyer.clone()),
            LEDGER_THRESHOLD,
            LEDGER_BUMP,
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use automint_bot_nft::{BotNFTContract, BotNFTContractClient};
    use automint_token::{AMTToken, AMTTokenClient};
    use soroban_sdk::{testutils::Address as _, Env, String};

    fn setup(env: &Env) -> (
        Address,
        BotNFTContractClient<'static>,
        AMTTokenClient<'static>,
        MarketplaceContractClient<'static>,
    ) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let bot_id = env.register_contract(None, BotNFTContract);
        let tok_id = env.register_contract(None, AMTToken);
        let mkt_id = env.register_contract(None, MarketplaceContract);
        let bot = BotNFTContractClient::new(env, &bot_id);
        let tok = AMTTokenClient::new(env, &tok_id);
        let mkt = MarketplaceContractClient::new(env, &mkt_id);
        bot.initialize(&admin);
        tok.initialize(
            &admin,
            &7u32,
            &String::from_str(env, "AutoMint Token"),
            &String::from_str(env, "AMT"),
        );
        mkt.initialize(&admin, &bot_id, &admin);
        (admin, bot, tok, mkt)
    }

    #[test]
    fn test_list_and_cancel() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let bot_id = bot.mint_basic(&seller).unwrap();
        mkt.list_bot(&seller, &bot_id, &0u32, &50_0000000_i128, &tok.address).unwrap();
        let listings = mkt.get_active_listings(&0u32, &10u32);
        assert_eq!(listings.len(), 1);
        assert_eq!(listings.get(0).unwrap().bot_id, bot_id);
        mkt.cancel_listing(&seller, &1u64).unwrap();
        assert_eq!(mkt.get_active_listings(&0u32, &10u32).len(), 0);
        assert_eq!(bot.get_user_bots(&seller).len(), 1);
    }

    #[test]
    fn test_buy_bot() {
        let env = Env::default();
        let (admin, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let buyer = Address::generate(&env);
        tok.mint(&buyer, &1000_0000000_i128);
        let bot_nft_id = bot.mint_basic(&seller).unwrap();
        mkt.list_bot(&seller, &bot_nft_id, &0u32, &100_0000000_i128, &tok.address).unwrap();
        mkt.buy_bot(&buyer, &1u64).unwrap();
        assert_eq!(bot.get_user_bots(&buyer).len(), 1);
        assert_eq!(bot.get_user_bots(&seller).len(), 0);
        // 2.5% fee goes to admin (fee_recipient)
        let fee = 100_0000000_i128 * 250 / 10_000;
        assert_eq!(tok.balance(&admin), fee);
        assert_eq!(tok.balance(&seller), 100_0000000_i128 - fee);
    }

    #[test]
    fn test_self_purchase_fails() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let bot_id = bot.mint_basic(&seller).unwrap();
        mkt.list_bot(&seller, &bot_id, &0u32, &50_0000000_i128, &tok.address).unwrap();
        assert!(mkt.try_buy_bot(&seller, &1u64).is_err());
    }

    #[test]
    fn test_zero_price_listing_fails() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let bot_id = bot.mint_basic(&seller).unwrap();
        assert!(mkt.try_list_bot(&seller, &bot_id, &0u32, &0_i128, &tok.address).is_err());
    }

    #[test]
    fn test_cancel_nonexistent_listing_fails() {
        let env = Env::default();
        let (_, _, _, mkt) = setup(&env);
        let user = Address::generate(&env);
        assert!(mkt.try_cancel_listing(&user, &999u64).is_err());
    }

    #[test]
    fn test_multiple_listings() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let id1 = bot.mint_basic(&seller).unwrap();
        let id2 = bot.mint_tier(&seller, &automint_bot_nft::BotTier::Bronze).unwrap();
        mkt.list_bot(&seller, &id1, &0u32, &10_0000000_i128, &tok.address).unwrap();
        mkt.list_bot(&seller, &id2, &1u32, &20_0000000_i128, &tok.address).unwrap();
        assert_eq!(mkt.get_active_listings(&0u32, &10u32).len(), 2);
        assert_eq!(mkt.get_user_listings(&seller).len(), 2);
    }

    #[test]
    fn test_double_initialize_fails() {
        let env = Env::default();
        let (admin, bot, _, mkt) = setup(&env);
        assert!(mkt.try_initialize(&admin, &bot.address, &admin).is_err());
    }

    #[test]
    fn test_user_purchase_history() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let buyer = Address::generate(&env);
        tok.mint(&buyer, &1000_0000000_i128);
        let bot_id = bot.mint_basic(&seller).unwrap();
        mkt.list_bot(&seller, &bot_id, &0u32, &100_0000000_i128, &tok.address).unwrap();
        mkt.buy_bot(&buyer, &1u64).unwrap();
        let purchases = mkt.get_user_purchases(&buyer, &10u32);
        assert_eq!(purchases.len(), 1);
        let purchase = purchases.get(0).unwrap();
        assert_eq!(purchase.listing_id, 1);
        assert_eq!(purchase.bot_id, bot_id);
        assert_eq!(purchase.seller, seller);
        assert_eq!(purchase.price, 100_0000000_i128);
    }

    #[test]
    fn test_bounded_purchase_history() {
        let env = Env::default();
        let (_, bot, tok, mkt) = setup(&env);
        let seller = Address::generate(&env);
        let buyer = Address::generate(&env);
        tok.mint(&buyer, &10000_0000000_i128);
        for i in 0u32..15 {
            let bot_id = bot.mint_basic(&seller).unwrap();
            mkt.list_bot(&seller, &bot_id, &0u32, &100_0000000_i128, &tok.address).unwrap();
            mkt.buy_bot(&buyer, &(i as u64 + 1)).unwrap();
        }
        let all_purchases = mkt.get_user_purchases(&buyer, &100u32);
        assert_eq!(all_purchases.len(), 15);
        let limited_purchases = mkt.get_user_purchases(&buyer, &5u32);
        assert_eq!(limited_purchases.len(), 5);
        let last_purchase = limited_purchases.get(4).unwrap();
        assert_eq!(last_purchase.listing_id, 15);
    }

    #[test]
    fn test_empty_purchase_history() {
        let env = Env::default();
        let (_, _, _, mkt) = setup(&env);
        let buyer = Address::generate(&env);
        let purchases = mkt.get_user_purchases(&buyer, &10u32);
        assert_eq!(purchases.len(), 0);
    }
}
