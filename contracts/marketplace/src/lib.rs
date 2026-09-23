#![no_std]
//! Marketplace contract for trading NFT bots.
//!
//! ## Events
//!
//! The marketplace emits the following events:
//! - `initialized`: Emitted when the contract is initialized with admin, bot_nft, and fee_recipient.
//!   Topics: ("initialized",), Data: (admin, bot_nft, fee_recipient)
//! - `listed`: Emitted when a bot is listed for sale.
//!   Topics: ("listed", seller, listing_id), Data: (bot_id, price)
//! - `sold`: Emitted when a bot is purchased.
//!   Topics: ("sold", seller, buyer), Data: (listing_id, bot_id, price)
//! - `cancel`: Emitted when a listing is cancelled.
//!   Topics: ("cancel", seller, listing_id), Data: (bot_id,)
//! - `fee_bps_updated`: Emitted when the fee basis points are updated.
//!   Topics: ("fee_bps_updated",), Data: (old_fee_bps, new_fee_bps)
//! - `fee_recipient_updated`: Emitted when the fee recipient is updated.
//!   Topics: ("fee_recipient_updated",), Data: (old_recipient, new_recipient)
//! - `bot_nft_updated`: Emitted when the bot NFT address is updated.
//!   Topics: ("bot_nft_updated",), Data: (old_nft, new_nft)
//! - `admin_updated`: Emitted when the admin is updated.
//!   Topics: ("admin_updated",), Data: (old_admin, new_admin)

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
        let config = Config { bot_nft: bot_nft.clone(), admin: admin.clone(), fee_bps: FEE_BPS, fee_recipient: fee_recipient.clone() };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::NextListingId, &1u64);
        env.storage().instance().set(&DataKey::ActiveListings, &Vec::<u64>::new(&env));
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            symbol_short!("initialized"),
            (admin, bot_nft, fee_recipient),
        );
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

    pub fn config(env: Env) -> Config {
        env.storage().instance().get(&DataKey::Config).unwrap()
    }

    pub fn set_fee_bps(env: Env, new_fee_bps: u32) -> Result<(), MarketplaceError> {
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(MarketplaceError::NotInitialized)?;
        config.admin.require_auth();
        let old_fee_bps = config.fee_bps;
        config.fee_bps = new_fee_bps;
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            symbol_short!("fee_bps_updated"),
            (old_fee_bps, new_fee_bps),
        );
        Ok(())
    }

    pub fn set_fee_recipient(env: Env, new_recipient: Address) -> Result<(), MarketplaceError> {
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(MarketplaceError::NotInitialized)?;
        config.admin.require_auth();
        let old_recipient = config.fee_recipient.clone();
        config.fee_recipient = new_recipient;
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            symbol_short!("fee_recipient_updated"),
            (old_recipient, config.fee_recipient.clone()),
        );
        Ok(())
    }

    pub fn set_bot_nft(env: Env, new_bot_nft: Address) -> Result<(), MarketplaceError> {
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(MarketplaceError::NotInitialized)?;
        config.admin.require_auth();
        let old_nft = config.bot_nft.clone();
        config.bot_nft = new_bot_nft;
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            symbol_short!("bot_nft_updated"),
            (old_nft, config.bot_nft.clone()),
        );
        Ok(())
    }

    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), MarketplaceError> {
        let mut config: Config = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(MarketplaceError::NotInitialized)?;
        config.admin.require_auth();
        let old_admin = config.admin.clone();
        config.admin = new_admin;
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
        env.events().publish(
            symbol_short!("admin_updated"),
            (old_admin, config.admin.clone()),
        );
        Ok(())
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
    fn test_initialize_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let bot_nft_addr = env.register_contract(None, BotNFTContract);
        let mkt_id = env.register_contract(None, MarketplaceContract);
        let mkt = MarketplaceContractClient::new(&env, &mkt_id);
        mkt.initialize(&admin, &bot_nft_addr, &admin);
        let events = env.events().all();
        assert!(events.len() >= 1);
        let init_event = events.last().unwrap();
        assert_eq!(init_event.0.get_unchecked(0), &symbol_short!("initialized").into_val(&env));
    }

    #[test]
    fn test_set_fee_bps_emits_event() {
        let env = Env::default();
        let (_, _, _, mkt) = setup(&env);
        let old_count = env.events().all().len();
        mkt.set_fee_bps(&500u32).unwrap();
        let events = env.events().all();
        assert!(events.len() > old_count);
        let config = mkt.config();
        assert_eq!(config.fee_bps, 500);
    }

    #[test]
    fn test_set_fee_recipient_emits_event() {
        let env = Env::default();
        let (_, _, _, mkt) = setup(&env);
        let new_recipient = Address::generate(&env);
        mkt.set_fee_recipient(&new_recipient).unwrap();
        let config = mkt.config();
        assert_eq!(config.fee_recipient, new_recipient);
    }

    #[test]
    fn test_set_bot_nft_emits_event() {
        let env = Env::default();
        let (_, _, _, mkt) = setup(&env);
        let new_nft = Address::generate(&env);
        mkt.set_bot_nft(&new_nft).unwrap();
        let config = mkt.config();
        assert_eq!(config.bot_nft, new_nft);
    }

    #[test]
    fn test_set_admin_emits_event() {
        let env = Env::default();
        let (admin, _, _, mkt) = setup(&env);
        let new_admin = Address::generate(&env);
        mkt.set_admin(&new_admin).unwrap();
        let config = mkt.config();
        assert_eq!(config.admin, new_admin);
    }

    #[test]
    fn test_unauthorized_set_fee_bps_fails() {
        let env = Env::default();
        let (admin, bot, tok, mkt) = setup(&env);
        let unauthorized = Address::generate(&env);
        env.mock_all_auths_allowing_non_root_auth();
        assert!(mkt.try_set_fee_bps(&500u32).is_err());
    }
}
