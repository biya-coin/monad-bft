pub mod cometbft {
    pub mod abci {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/cometbft.abci.v1.rs"));
        }
    }

    pub mod crypto {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/cometbft.crypto.v1.rs"));
        }
    }

    pub mod types {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/cometbft.types.v1.rs"));
        }
    }

    pub mod version {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/cometbft.version.v1.rs"));
        }
    }
}

pub mod descriptor {
    include!(concat!(env!("OUT_DIR"), "/cometbft_abci_descriptor.rs"));
}
