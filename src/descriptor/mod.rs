// Magical Bitcoin Library
// Written in 2020 by
//     Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020 Magical Bitcoin
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Descriptors
//!
//! This module contains generic utilities to work with descriptors, plus some re-exported types
//! from [`miniscript`].

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;

use bitcoin::util::bip32::{
    ChildNumber, DerivationPath, ExtendedPrivKey, ExtendedPubKey, Fingerprint, KeySource,
};
use bitcoin::util::psbt;
use bitcoin::{Network, PublicKey, Script, TxOut};

use miniscript::descriptor::{DescriptorPublicKey, DescriptorType, DescriptorXKey, Wildcard};
pub use miniscript::{descriptor::KeyMap, Descriptor, Legacy, Miniscript, ScriptContext, Segwitv0};
use miniscript::{DescriptorTrait, ForEachKey, TranslatePk};

pub mod checksum;
pub(crate) mod derived;
#[doc(hidden)]
pub mod dsl;
pub mod error;
pub mod policy;
pub mod template;

pub use self::checksum::get_checksum;
use self::derived::AsDerived;
pub use self::derived::DerivedDescriptorKey;
pub use self::error::Error as DescriptorError;
pub use self::policy::Policy;
use self::template::DescriptorTemplateOut;
use crate::keys::{IntoDescriptorKey, KeyError};
use crate::wallet::signer::SignersContainer;
use crate::wallet::utils::SecpCtx;

/// Alias for a [`Descriptor`] that can contain extended keys using [`DescriptorPublicKey`]
pub type ExtendedDescriptor = Descriptor<DescriptorPublicKey>;

/// Alias for a [`Descriptor`] that contains extended **derived** keys
pub type DerivedDescriptor<'s> = Descriptor<DerivedDescriptorKey<'s>>;

/// Alias for the type of maps that represent derivation paths in a [`psbt::Input`] or
/// [`psbt::Output`]
///
/// [`psbt::Input`]: bitcoin::util::psbt::Input
/// [`psbt::Output`]: bitcoin::util::psbt::Output
pub type HDKeyPaths = BTreeMap<PublicKey, KeySource>;

/// Trait for types which can be converted into an [`ExtendedDescriptor`] and a [`KeyMap`] usable by a wallet in a specific [`Network`]
pub trait IntoWalletDescriptor {
    /// Convert to wallet descriptor
    fn into_wallet_descriptor(
        self,
        secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError>;
}

impl IntoWalletDescriptor for &str {
    fn into_wallet_descriptor(
        self,
        secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
        let descriptor = if self.contains('#') {
            let parts: Vec<&str> = self.splitn(2, '#').collect();
            if !get_checksum(parts[0])
                .ok()
                .map(|computed| computed == parts[1])
                .unwrap_or(false)
            {
                return Err(DescriptorError::InvalidDescriptorChecksum);
            }

            parts[0]
        } else {
            self
        };

        ExtendedDescriptor::parse_descriptor(secp, descriptor)?
            .into_wallet_descriptor(secp, network)
    }
}

impl IntoWalletDescriptor for &String {
    fn into_wallet_descriptor(
        self,
        secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
        self.as_str().into_wallet_descriptor(secp, network)
    }
}

impl IntoWalletDescriptor for ExtendedDescriptor {
    fn into_wallet_descriptor(
        self,
        secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
        (self, KeyMap::default()).into_wallet_descriptor(secp, network)
    }
}

impl IntoWalletDescriptor for (ExtendedDescriptor, KeyMap) {
    fn into_wallet_descriptor(
        self,
        secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
        use crate::keys::DescriptorKey;

        let check_key = |pk: &DescriptorPublicKey| {
            let (pk, _, networks) = if self.0.is_witness() {
                let desciptor_key: DescriptorKey<miniscript::Segwitv0> =
                    pk.clone().into_descriptor_key()?;
                desciptor_key.extract(&secp)?
            } else {
                let desciptor_key: DescriptorKey<miniscript::Legacy> =
                    pk.clone().into_descriptor_key()?;
                desciptor_key.extract(&secp)?
            };

            if networks.contains(&network) {
                Ok(pk)
            } else {
                Err(DescriptorError::Key(KeyError::InvalidNetwork))
            }
        };

        // check the network for the keys
        let translated = self.0.translate_pk(check_key, check_key)?;

        Ok((translated, self.1))
    }
}

impl IntoWalletDescriptor for DescriptorTemplateOut {
    fn into_wallet_descriptor(
        self,
        _secp: &SecpCtx,
        network: Network,
    ) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
        let valid_networks = &self.2;

        let fix_key = |pk: &DescriptorPublicKey| {
            if valid_networks.contains(&network) {
                // workaround for xpubs generated by other key types, like bip39: since when the
                // conversion is made one network has to be chosen, what we generally choose
                // "mainnet", but then override the set of valid networks to specify that all of
                // them are valid. here we reset the network to make sure the wallet struct gets a
                // descriptor with the right network everywhere.
                let pk = match pk {
                    DescriptorPublicKey::XPub(ref xpub) => {
                        let mut xpub = xpub.clone();
                        xpub.xkey.network = network;

                        DescriptorPublicKey::XPub(xpub)
                    }
                    other => other.clone(),
                };

                Ok(pk)
            } else {
                Err(DescriptorError::Key(KeyError::InvalidNetwork))
            }
        };

        // fixup the network for keys that need it
        let translated = self.0.translate_pk(fix_key, fix_key)?;

        Ok((translated, self.1))
    }
}

/// Wrapper for `IntoWalletDescriptor` that performs additional checks on the keys contained in the
/// descriptor
pub(crate) fn into_wallet_descriptor_checked<T: IntoWalletDescriptor>(
    inner: T,
    secp: &SecpCtx,
    network: Network,
) -> Result<(ExtendedDescriptor, KeyMap), DescriptorError> {
    let (descriptor, keymap) = inner.into_wallet_descriptor(secp, network)?;

    // Ensure the keys don't contain any hardened derivation steps or hardened wildcards
    let descriptor_contains_hardened_steps = descriptor.for_any_key(|k| {
        if let DescriptorPublicKey::XPub(DescriptorXKey {
            derivation_path,
            wildcard,
            ..
        }) = k.as_key()
        {
            return *wildcard == Wildcard::Hardened
                || derivation_path.into_iter().any(ChildNumber::is_hardened);
        }

        false
    });
    if descriptor_contains_hardened_steps {
        return Err(DescriptorError::HardenedDerivationXpub);
    }

    Ok((descriptor, keymap))
}

#[doc(hidden)]
/// Used internally mainly by the `descriptor!()` and `fragment!()` macros
pub trait CheckMiniscript<Ctx: miniscript::ScriptContext> {
    fn check_minsicript(&self) -> Result<(), miniscript::Error>;
}

impl<Ctx: miniscript::ScriptContext, Pk: miniscript::MiniscriptKey> CheckMiniscript<Ctx>
    for miniscript::Miniscript<Pk, Ctx>
{
    fn check_minsicript(&self) -> Result<(), miniscript::Error> {
        Ctx::check_global_validity(self)?;

        Ok(())
    }
}

/// Trait implemented on [`Descriptor`]s to add a method to extract the spending [`policy`]
pub trait ExtractPolicy {
    /// Extract the spending [`policy`]
    fn extract_policy(
        &self,
        signers: &SignersContainer,
        secp: &SecpCtx,
    ) -> Result<Option<Policy>, DescriptorError>;
}

pub(crate) trait XKeyUtils {
    fn full_path(&self, append: &[ChildNumber]) -> DerivationPath;
    fn root_fingerprint(&self, secp: &SecpCtx) -> Fingerprint;
}

// FIXME: `InnerXKey` was made private in rust-miniscript, so we have to implement this manually on
// both `ExtendedPubKey` and `ExtendedPrivKey`.
//
// Revert back to using the trait once https://github.com/rust-bitcoin/rust-miniscript/pull/230 is
// released
impl XKeyUtils for DescriptorXKey<ExtendedPubKey> {
    fn full_path(&self, append: &[ChildNumber]) -> DerivationPath {
        let full_path = match self.origin {
            Some((_, ref path)) => path
                .into_iter()
                .chain(self.derivation_path.into_iter())
                .cloned()
                .collect(),
            None => self.derivation_path.clone(),
        };

        if self.wildcard != Wildcard::None {
            full_path
                .into_iter()
                .chain(append.iter())
                .cloned()
                .collect()
        } else {
            full_path
        }
    }

    fn root_fingerprint(&self, _: &SecpCtx) -> Fingerprint {
        match self.origin {
            Some((fingerprint, _)) => fingerprint,
            None => self.xkey.fingerprint(),
        }
    }
}
impl XKeyUtils for DescriptorXKey<ExtendedPrivKey> {
    fn full_path(&self, append: &[ChildNumber]) -> DerivationPath {
        let full_path = match self.origin {
            Some((_, ref path)) => path
                .into_iter()
                .chain(self.derivation_path.into_iter())
                .cloned()
                .collect(),
            None => self.derivation_path.clone(),
        };

        if self.wildcard != Wildcard::None {
            full_path
                .into_iter()
                .chain(append.iter())
                .cloned()
                .collect()
        } else {
            full_path
        }
    }

    fn root_fingerprint(&self, secp: &SecpCtx) -> Fingerprint {
        match self.origin {
            Some((fingerprint, _)) => fingerprint,
            None => self.xkey.fingerprint(secp),
        }
    }
}

pub(crate) trait DerivedDescriptorMeta {
    fn get_hd_keypaths(&self, secp: &SecpCtx) -> Result<HDKeyPaths, DescriptorError>;
}

pub(crate) trait DescriptorMeta {
    fn is_witness(&self) -> bool;
    fn get_extended_keys(&self) -> Result<Vec<DescriptorXKey<ExtendedPubKey>>, DescriptorError>;
    fn derive_from_hd_keypaths<'s>(
        &self,
        hd_keypaths: &HDKeyPaths,
        utxo: &Option<TxOut>,
        secp: &'s SecpCtx,
    ) -> Option<DerivedDescriptor<'s>>;
    fn derive_from_psbt_input<'s>(
        &self,
        psbt_input: &psbt::Input,
        utxo: Option<TxOut>,
        secp: &'s SecpCtx,
    ) -> Option<DerivedDescriptor<'s>>;
}

pub(crate) trait DescriptorScripts {
    fn psbt_redeem_script(&self) -> Option<Script>;
    fn psbt_witness_script(&self) -> Option<Script>;
}

impl<'s> DescriptorScripts for DerivedDescriptor<'s> {
    fn psbt_redeem_script(&self) -> Option<Script> {
        match self.desc_type() {
            DescriptorType::ShWpkh => Some(self.explicit_script()),
            DescriptorType::ShWsh => Some(self.explicit_script().to_v0_p2wsh()),
            DescriptorType::Sh => Some(self.explicit_script()),
            DescriptorType::Bare => Some(self.explicit_script()),
            DescriptorType::ShSortedMulti => Some(self.explicit_script()),
            _ => None,
        }
    }

    fn psbt_witness_script(&self) -> Option<Script> {
        match self.desc_type() {
            DescriptorType::Wsh => Some(self.explicit_script()),
            DescriptorType::ShWsh => Some(self.explicit_script()),
            DescriptorType::WshSortedMulti | DescriptorType::ShWshSortedMulti => {
                Some(self.explicit_script())
            }
            _ => None,
        }
    }
}

impl DescriptorMeta for ExtendedDescriptor {
    fn is_witness(&self) -> bool {
        matches!(
            self.desc_type(),
            DescriptorType::Wpkh
                | DescriptorType::ShWpkh
                | DescriptorType::Wsh
                | DescriptorType::ShWsh
                | DescriptorType::ShWshSortedMulti
                | DescriptorType::WshSortedMulti
        )
    }

    fn get_extended_keys(&self) -> Result<Vec<DescriptorXKey<ExtendedPubKey>>, DescriptorError> {
        let mut answer = Vec::new();

        self.for_each_key(|pk| {
            if let DescriptorPublicKey::XPub(xpub) = pk.as_key() {
                answer.push(xpub.clone());
            }

            true
        });

        Ok(answer)
    }

    fn derive_from_hd_keypaths<'s>(
        &self,
        hd_keypaths: &HDKeyPaths,
        utxo: &Option<TxOut>,
        secp: &'s SecpCtx,
    ) -> Option<DerivedDescriptor<'s>> {
        let index: HashMap<_, Vec<_>> =
            hd_keypaths
                .values()
                .fold(HashMap::new(), |mut map, (f, p)| {
                    map.entry(f).or_default().push(p);
                    map
                });

        let mut descriptor_found = None;
        self.for_each_key(|key| {
            if descriptor_found.is_some() {
                // already found a matching path, we are done
                return true;
            }

            if let DescriptorPublicKey::XPub(xpub) = key.as_key().deref() {
                // Ignore non-wildcard keys, since they are effectively "fixed". If a descriptor
                // only has non-wildcard keys, then it's fixed and doesn't need this part. On the
                // other end, if there are wildcard keys we should only consider those since they
                // are the ones that contain the actual derivation index.
                if xpub.wildcard == Wildcard::None {
                    return false;
                }

                let root_fingerprint = xpub.root_fingerprint(secp);
                let paths = match index.get(&root_fingerprint) {
                    Some(paths) => paths,
                    None => return false,
                };

                // Check if the key matches one entry in our `index`. If it does, `matches()` will
                // return the "prefix" that matched, so we remove that prefix from the full path
                // found in `index`. We expect this to be a derivation path of length 1 because
                // the key is `wildcard`
                for path in paths {
                    let prefix = match xpub.matches(&(root_fingerprint, (*path).clone()), secp) {
                        Some(prefix) => prefix,
                        _ => continue,
                    };
                    let path_without_prefix: Vec<_> = path
                        .into_iter()
                        .skip(prefix.into_iter().count())
                        .cloned()
                        .collect();

                    // Only consider paths of length 1 and with a normal step
                    if let &[ChildNumber::Normal { index }] = path_without_prefix.as_slice() {
                        let descriptor = self.as_derived(index, secp);

                        // If we have the UTXO double check by generating the script_pubkey for
                        // the descriptor and comparing it
                        if let Some(utxo) = utxo {
                            if descriptor.script_pubkey() != utxo.script_pubkey {
                                return false;
                            }
                        }

                        descriptor_found = Some(descriptor)
                    }
                }
            }

            false
        });

        descriptor_found
    }

    fn derive_from_psbt_input<'s>(
        &self,
        psbt_input: &psbt::Input,
        utxo: Option<TxOut>,
        secp: &'s SecpCtx,
    ) -> Option<DerivedDescriptor<'s>> {
        if let Some(derived) =
            self.derive_from_hd_keypaths(&psbt_input.bip32_derivation, &utxo, secp)
        {
            return Some(derived);
        }
        if self.is_deriveable() {
            // We can't try to bruteforce the derivation index, exit here
            return None;
        }

        let descriptor = self.as_derived_fixed(secp);
        match descriptor.desc_type() {
            // TODO: add pk() here
            DescriptorType::Pkh | DescriptorType::Wpkh | DescriptorType::ShWpkh
                if utxo.is_some()
                    && descriptor.script_pubkey() == utxo.as_ref().unwrap().script_pubkey =>
            {
                Some(descriptor)
            }
            DescriptorType::Bare | DescriptorType::Sh | DescriptorType::ShSortedMulti
                if psbt_input.redeem_script.is_some()
                    && &descriptor.explicit_script()
                        == psbt_input.redeem_script.as_ref().unwrap() =>
            {
                Some(descriptor)
            }
            DescriptorType::Wsh
            | DescriptorType::ShWsh
            | DescriptorType::ShWshSortedMulti
            | DescriptorType::WshSortedMulti
                if psbt_input.witness_script.is_some()
                    && &descriptor.explicit_script()
                        == psbt_input.witness_script.as_ref().unwrap() =>
            {
                Some(descriptor)
            }
            _ => None,
        }
    }
}

impl<'s> DerivedDescriptorMeta for DerivedDescriptor<'s> {
    fn get_hd_keypaths(&self, secp: &SecpCtx) -> Result<HDKeyPaths, DescriptorError> {
        let mut answer = BTreeMap::new();
        self.for_each_key(|key| {
            if let DescriptorPublicKey::XPub(xpub) = key.as_key().deref() {
                let derived_pubkey = xpub
                    .xkey
                    .derive_pub(secp, &xpub.derivation_path)
                    .expect("Derivation can't fail");

                answer.insert(
                    derived_pubkey.public_key,
                    (xpub.root_fingerprint(secp), xpub.full_path(&[])),
                );
            }

            true
        });

        Ok(answer)
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use bitcoin::consensus::encode::deserialize;
    use bitcoin::hashes::hex::FromHex;
    use bitcoin::secp256k1::Secp256k1;
    use bitcoin::util::{bip32, psbt};

    use super::*;
    use crate::psbt::PSBTUtils;

    #[test]
    fn test_derive_from_psbt_input_wpkh_wif() {
        let descriptor = Descriptor::<DescriptorPublicKey>::from_str(
            "wpkh(02b4632d08485ff1df2db55b9dafd23347d1c47a457072a1e87be26896549a8737)",
        )
        .unwrap();
        let psbt: psbt::PartiallySignedTransaction = deserialize(
            &Vec::<u8>::from_hex(
                "70736274ff010052010000000162307be8e431fbaff807cdf9cdc3fde44d7402\
                 11bc8342c31ffd6ec11fe35bcc0100000000ffffffff01328601000000000016\
                 001493ce48570b55c42c2af816aeaba06cfee1224fae000000000001011fa086\
                 01000000000016001493ce48570b55c42c2af816aeaba06cfee1224fae010304\
                 010000000000",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(descriptor
            .derive_from_psbt_input(&psbt.inputs[0], psbt.get_utxo_for(0), &Secp256k1::new())
            .is_some());
    }

    #[test]
    fn test_derive_from_psbt_input_pkh_tpub() {
        let descriptor = Descriptor::<DescriptorPublicKey>::from_str(
            "pkh([0f056943/44h/0h/0h]tpubDDpWvmUrPZrhSPmUzCMBHffvC3HyMAPnWDSAQNBTnj1iZeJa7BZQEttFiP4DS4GCcXQHezdXhn86Hj6LHX5EDstXPWrMaSneRWM8yUf6NFd/10/*)",
        )
        .unwrap();
        let psbt: psbt::PartiallySignedTransaction = deserialize(
            &Vec::<u8>::from_hex(
                "70736274ff010053010000000145843b86be54a3cd8c9e38444e1162676c00df\
                 e7964122a70df491ea12fd67090100000000ffffffff01c19598000000000017\
                 a91432bb94283282f72b2e034709e348c44d5a4db0ef8700000000000100f902\
                 0000000001010167e99c0eb67640f3a1b6805f2d8be8238c947f8aaf49eb0a9c\
                 bee6a42c984200000000171600142b29a22019cca05b9c2b2d283a4c4489e1cf\
                 9f8ffeffffff02a01dced06100000017a914e2abf033cadbd74f0f4c74946201\
                 decd20d5c43c8780969800000000001976a9148b0fce5fb1264e599a65387313\
                 3c95478b902eb288ac02473044022015d9211576163fa5b001e84dfa3d44efd9\
                 86b8f3a0d3d2174369288b2b750906022048dacc0e5d73ae42512fd2b97e2071\
                 a8d0bce443b390b1fe0b8128fe70ec919e01210232dad1c5a67dcb0116d407e2\
                 52584228ab7ec00e8b9779d0c3ffe8114fc1a7d2c80600000103040100000022\
                 0603433b83583f8c4879b329dd08bbc7da935e4cc02f637ff746e05f0466ffb2\
                 a6a2180f0569432c00008000000080000000800a000000000000000000",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(descriptor
            .derive_from_psbt_input(&psbt.inputs[0], psbt.get_utxo_for(0), &Secp256k1::new())
            .is_some());
    }

    #[test]
    fn test_derive_from_psbt_input_wsh() {
        let descriptor = Descriptor::<DescriptorPublicKey>::from_str(
            "wsh(and_v(v:pk(03b6633fef2397a0a9de9d7b6f23aef8368a6e362b0581f0f0af70d5ecfd254b14),older(6)))",
        )
        .unwrap();
        let psbt: psbt::PartiallySignedTransaction = deserialize(
            &Vec::<u8>::from_hex(
                "70736274ff01005302000000011c8116eea34408ab6529223c9a176606742207\
                 67a1ff1d46a6e3c4a88243ea6e01000000000600000001109698000000000017\
                 a914ad105f61102e0d01d7af40d06d6a5c3ae2f7fde387000000000001012b80\
                 969800000000002200203ca72f106a72234754890ca7640c43f65d2174e44d33\
                 336030f9059345091044010304010000000105252103b6633fef2397a0a9de9d\
                 7b6f23aef8368a6e362b0581f0f0af70d5ecfd254b14ad56b20000",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(descriptor
            .derive_from_psbt_input(&psbt.inputs[0], psbt.get_utxo_for(0), &Secp256k1::new())
            .is_some());
    }

    #[test]
    fn test_derive_from_psbt_input_sh() {
        let descriptor = Descriptor::<DescriptorPublicKey>::from_str(
            "sh(and_v(v:pk(021403881a5587297818fcaf17d239cefca22fce84a45b3b1d23e836c4af671dbb),after(630000)))",
        )
        .unwrap();
        let psbt: psbt::PartiallySignedTransaction = deserialize(
            &Vec::<u8>::from_hex(
                "70736274ff0100530100000001bc8c13df445dfadcc42afa6dc841f85d22b01d\
                 a6270ebf981740f4b7b1d800390000000000feffffff01ba9598000000000017\
                 a91457b148ba4d3e5fa8608a8657875124e3d1c9390887f09c0900000100e002\
                 0000000001016ba1bbe05cc93574a0d611ec7d93ad0ab6685b28d0cd80e8a82d\
                 debb326643c90100000000feffffff02809698000000000017a914d9a6e8c455\
                 8e16c8253afe53ce37ad61cf4c38c487403504cf6100000017a9144044fb6e0b\
                 757dfc1b34886b6a95aef4d3db137e870247304402202a9b72d939bcde8ba2a1\
                 e0980597e47af4f5c152a78499143c3d0a78ac2286a602207a45b1df9e93b8c9\
                 6f09f5c025fe3e413ca4b905fe65ee55d32a3276439a9b8f012102dc1fcc2636\
                 4da1aa718f03d8d9bd6f2ff410ed2cf1245a168aa3bcc995ac18e0a806000001\
                 03040100000001042821021403881a5587297818fcaf17d239cefca22fce84a4\
                 5b3b1d23e836c4af671dbbad03f09c09b10000",
            )
            .unwrap(),
        )
        .unwrap();

        assert!(descriptor
            .derive_from_psbt_input(&psbt.inputs[0], psbt.get_utxo_for(0), &Secp256k1::new())
            .is_some());
    }

    #[test]
    fn test_derive_from_psbt_input_edge_case_same_key() {
        use std::str::FromStr;

        let secp = Secp256k1::new();

        // Our descriptor contains the same key, once with a wildcard path m/0/* and once with a
        // fixed path of m/0/5
        let (descriptor, _) = Descriptor::<DescriptorPublicKey>::parse_descriptor(
            &secp, "sh(and_v(v:pk(tprv8ZgxMBicQKsPdEpgG7Bo5seco4WuhQJpX4soNr8AKx5fWsgQHXDQVAYCT5z5zd1JSQyzSAbF6m1UtdTMX9wyXY13HDamRuzLeHkSHNviQZB/0/*),pk(tprv8ZgxMBicQKsPdEpgG7Bo5seco4WuhQJpX4soNr8AKx5fWsgQHXDQVAYCT5z5zd1JSQyzSAbF6m1UtdTMX9wyXY13HDamRuzLeHkSHNviQZB/0/5)))",
        )
        .unwrap();

        // This psbt spends and input from address #1, which means that the keys have paths m/0/1
        // and m/0/5. We should correctly detect that this is address #1 instead of #5
        let psbt: psbt::PartiallySignedTransaction = deserialize(
            &Vec::<u8>::from_hex(
                "70736274ff010055010000000155a951caabf3adc17011b50bf7b5277bd8\
                 7984eadb9c7148a7794f976575f47f0000000000ffffffff01e125000000\
                 0000001976a914344a0f48ca150ec2b903817660b9b68b13a6702688ac00\
                 000000000100e002000000000101b02bb8a1253af4dc685e8ae0fa6fd701\
                 2c714ece6234814cfaad31b212aec5680100000000feffffff0210270000\
                 0000000017a9147612e375ade0fe6511b965122c4abac9e8c71aff870731\
                 14000000000017a9140d0feac1f69a04467f8308dc0cfe4a45cafb092987\
                 02473044022035cf76f783fd555c9ec6fdebc5209656217dd89c2c6647ff\
                 0a0b44fe020ec62f02201bb328762f2006d42ab4f9c5fbc1a0a062975be3\
                 7ec51abafdd76ee1580f59030121038caa50b1ad30300a3c84f07c6e089d\
                 90a6f32ec6a12f0753db1529a0df97cb7426901d00010446210230a5ccbf\
                 715c41b497a849381fb6070afb8246e94edaf83b3934ae9b758dd68ead21\
                 0377d07b8d74ade4d9872ba91b535e44add4e33c3c5b498143d858e38e51\
                 dcbd82ac22060230a5ccbf715c41b497a849381fb6070afb8246e94edaf8\
                 3b3934ae9b758dd68e0c43315493000000000100000022060377d07b8d74\
                 ade4d9872ba91b535e44add4e33c3c5b498143d858e38e51dcbd820c4331\
                 549300000000050000000000",
            )
            .unwrap(),
        )
        .unwrap();

        let found = descriptor.derive_from_psbt_input(&psbt.inputs[0], psbt.get_utxo_for(0), &secp);

        assert!(found.is_some());
        assert_eq!(
            found.unwrap().script_pubkey(),
            Script::from_str("a9147612e375ade0fe6511b965122c4abac9e8c71aff87").unwrap()
        );
    }

    #[test]
    fn test_to_wallet_descriptor_fixup_networks() {
        use crate::keys::{any_network, IntoDescriptorKey};

        let secp = Secp256k1::new();

        let xpub = bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap();
        let path = bip32::DerivationPath::from_str("m/0").unwrap();

        // here `to_descriptor_key` will set the valid networks for the key to only mainnet, since
        // we are using an "xpub"
        let key = (xpub, path).into_descriptor_key().unwrap();
        // override it with any. this happens in some key conversions, like bip39
        let key = key.override_valid_networks(any_network());

        // make a descriptor out of it
        let desc = crate::descriptor!(wpkh(key)).unwrap();
        // this should conver the key that supports "any_network" to the right network (testnet)
        let (wallet_desc, _) = desc
            .into_wallet_descriptor(&secp, Network::Testnet)
            .unwrap();

        assert_eq!(wallet_desc.to_string(), "wpkh(tpubDEnoLuPdBep9bzw5LoGYpsxUQYheRQ9gcgrJhJEcdKFB9cWQRyYmkCyRoTqeD4tJYiVVgt6A3rN6rWn9RYhR9sBsGxji29LYWHuKKbdb1ev/0/*)#y8p7e8kk");
    }

    // test IntoWalletDescriptor trait from &str with and without checksum appended
    #[test]
    fn test_descriptor_from_str_with_checksum() {
        let secp = Secp256k1::new();

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)#tqz0nc62"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)#67ju93jw"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)#67ju93jw"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(matches!(
            desc.err(),
            Some(DescriptorError::InvalidDescriptorChecksum)
        ));

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)#67ju93jw"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(matches!(
            desc.err(),
            Some(DescriptorError::InvalidDescriptorChecksum)
        ));
    }

    // test IntoWalletDescriptor trait from &str with keys from right and wrong network
    #[test]
    fn test_descriptor_from_str_with_keys_network() {
        let secp = Secp256k1::new();

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Regtest);
        assert!(desc.is_ok());

        let desc = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Regtest);
        assert!(desc.is_ok());

        let desc = "sh(wpkh(02864bb4ad00cefa806098a69e192bbda937494e69eb452b87bb3f20f6283baedb))"
            .into_wallet_descriptor(&secp, Network::Testnet);
        assert!(desc.is_ok());

        let desc = "sh(wpkh(02864bb4ad00cefa806098a69e192bbda937494e69eb452b87bb3f20f6283baedb))"
            .into_wallet_descriptor(&secp, Network::Bitcoin);
        assert!(desc.is_ok());

        let desc = "wpkh(tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Bitcoin);
        assert!(matches!(
            desc.err(),
            Some(DescriptorError::Key(KeyError::InvalidNetwork))
        ));

        let desc = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)"
            .into_wallet_descriptor(&secp, Network::Bitcoin);
        assert!(matches!(
            desc.err(),
            Some(DescriptorError::Key(KeyError::InvalidNetwork))
        ));
    }

    // test IntoWalletDescriptor trait from the output of the descriptor!() macro
    #[test]
    fn test_descriptor_from_str_from_output_of_macro() {
        let secp = Secp256k1::new();

        let tpub = bip32::ExtendedPubKey::from_str("tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK").unwrap();
        let path = bip32::DerivationPath::from_str("m/1/2").unwrap();
        let key = (tpub, path).into_descriptor_key().unwrap();

        // make a descriptor out of it
        let desc = crate::descriptor!(wpkh(key)).unwrap();

        let (wallet_desc, _) = desc
            .into_wallet_descriptor(&secp, Network::Testnet)
            .unwrap();
        let wallet_desc_str = wallet_desc.to_string();
        assert_eq!(wallet_desc_str, "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/1/2/*)#67ju93jw");

        let (wallet_desc2, _) = wallet_desc_str
            .into_wallet_descriptor(&secp, Network::Testnet)
            .unwrap();
        assert_eq!(wallet_desc, wallet_desc2)
    }

    #[test]
    fn test_into_wallet_descriptor_checked() {
        let secp = Secp256k1::new();

        let descriptor = "wpkh(tpubD6NzVbkrYhZ4XHndKkuB8FifXm8r5FQHwrN6oZuWCz13qb93rtgKvD4PQsqC4HP4yhV3tA2fqr2RbY5mNXfM7RxXUoeABoDtsFUq2zJq6YK/0'/1/2/*)";
        let result = into_wallet_descriptor_checked(descriptor, &secp, Network::Testnet);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DescriptorError::HardenedDerivationXpub
        ));
    }
}
