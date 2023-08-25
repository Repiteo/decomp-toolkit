use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    ops::{Index, RangeBounds},
};

use anyhow::{anyhow, bail, ensure, Result};
use flagset::{flags, FlagSet};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::{
    analysis::cfa::SectionAddress,
    obj::{ObjKind, ObjRelocKind},
    util::{config::is_auto_symbol, nested::NestedVec, split::is_linker_generated_label},
};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum ObjSymbolScope {
    #[default]
    Unknown,
    Global,
    Weak,
    Local,
}

flags! {
    #[repr(u8)]
    #[derive(Deserialize_repr, Serialize_repr)]
    pub enum ObjSymbolFlags: u8 {
        Global,
        Local,
        Weak,
        Common,
        Hidden,
        ForceActive,
        /// Symbol isn't referenced by any relocations
        RelocationIgnore,
    }
}

#[derive(Debug, Copy, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjSymbolFlagSet(pub FlagSet<ObjSymbolFlags>);

impl ObjSymbolFlagSet {
    #[inline]
    pub fn scope(&self) -> ObjSymbolScope {
        if self.is_local() {
            ObjSymbolScope::Local
        } else if self.is_weak() {
            ObjSymbolScope::Weak
        } else if self.0.contains(ObjSymbolFlags::Global) {
            ObjSymbolScope::Global
        } else {
            ObjSymbolScope::Unknown
        }
    }

    #[inline]
    pub fn is_local(&self) -> bool { self.0.contains(ObjSymbolFlags::Local) }

    #[inline]
    pub fn is_global(&self) -> bool { !self.is_local() }

    #[inline]
    pub fn is_common(&self) -> bool { self.0.contains(ObjSymbolFlags::Common) }

    #[inline]
    pub fn is_weak(&self) -> bool { self.0.contains(ObjSymbolFlags::Weak) }

    #[inline]
    pub fn is_hidden(&self) -> bool { self.0.contains(ObjSymbolFlags::Hidden) }

    #[inline]
    pub fn is_force_active(&self) -> bool { self.0.contains(ObjSymbolFlags::ForceActive) }

    #[inline]
    pub fn is_relocation_ignore(&self) -> bool { self.0.contains(ObjSymbolFlags::RelocationIgnore) }

    #[inline]
    pub fn set_scope(&mut self, scope: ObjSymbolScope) {
        match scope {
            ObjSymbolScope::Unknown => {
                self.0 &= !(ObjSymbolFlags::Local | ObjSymbolFlags::Global | ObjSymbolFlags::Weak)
            }
            ObjSymbolScope::Global => {
                self.0 = (self.0 & !(ObjSymbolFlags::Local | ObjSymbolFlags::Weak))
                    | ObjSymbolFlags::Global
            }
            ObjSymbolScope::Weak => {
                self.0 = (self.0 & !(ObjSymbolFlags::Local | ObjSymbolFlags::Global))
                    | ObjSymbolFlags::Weak
            }
            ObjSymbolScope::Local => {
                self.0 = (self.0 & !(ObjSymbolFlags::Global | ObjSymbolFlags::Weak))
                    | ObjSymbolFlags::Local
            }
        }
    }

    #[inline]
    pub fn set_force_active(&mut self, value: bool) {
        if value {
            self.0 |= ObjSymbolFlags::ForceActive;
        } else {
            self.0 &= !ObjSymbolFlags::ForceActive;
        }
    }
}

#[allow(clippy::derived_hash_with_manual_eq)]
impl Hash for ObjSymbolFlagSet {
    fn hash<H: Hasher>(&self, state: &mut H) { self.0.bits().hash(state) }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Default, Serialize, Deserialize)]
pub enum ObjSymbolKind {
    #[default]
    Unknown,
    Function,
    Object,
    Section,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum ObjDataKind {
    #[default]
    Unknown,
    Byte,
    Byte2,
    Byte4,
    Byte8,
    Float,
    Double,
    String,
    String16,
    StringTable,
    String16Table,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ObjSymbol {
    pub name: String,
    pub demangled_name: Option<String>,
    pub address: u64,
    pub section: Option<usize>,
    pub size: u64,
    pub size_known: bool,
    pub flags: ObjSymbolFlagSet,
    pub kind: ObjSymbolKind,
    pub align: Option<u32>,
    pub data_kind: ObjDataKind,
}

pub type SymbolIndex = usize;

#[derive(Debug, Clone)]
pub struct ObjSymbols {
    obj_kind: ObjKind,
    symbols: Vec<ObjSymbol>,
    symbols_by_address: BTreeMap<u32, Vec<SymbolIndex>>,
    symbols_by_name: HashMap<String, Vec<SymbolIndex>>,
    symbols_by_section: Vec<BTreeMap<u32, Vec<SymbolIndex>>>,
}

impl ObjSymbols {
    pub fn new(obj_kind: ObjKind, symbols: Vec<ObjSymbol>) -> Self {
        let mut symbols_by_address = BTreeMap::<u32, Vec<SymbolIndex>>::new();
        let mut symbols_by_section: Vec<BTreeMap<u32, Vec<SymbolIndex>>> = vec![];
        let mut symbols_by_name = HashMap::<String, Vec<SymbolIndex>>::new();
        for (idx, symbol) in symbols.iter().enumerate() {
            symbols_by_address.nested_push(symbol.address as u32, idx);
            if let Some(section_idx) = symbol.section {
                if section_idx >= symbols_by_section.len() {
                    symbols_by_section.resize_with(section_idx + 1, BTreeMap::new);
                }
                symbols_by_section[section_idx].nested_push(symbol.address as u32, idx);
            } else {
                debug_assert!(
                    symbol.address == 0
                        || symbol.flags.is_common()
                        || obj_kind == ObjKind::Executable,
                    "ABS symbol in relocatable object"
                );
            }
            if !symbol.name.is_empty() {
                symbols_by_name.nested_push(symbol.name.clone(), idx);
            }
        }
        Self { obj_kind, symbols, symbols_by_address, symbols_by_name, symbols_by_section }
    }

    pub fn add(&mut self, in_symbol: ObjSymbol, replace: bool) -> Result<SymbolIndex> {
        let opt = if let Some(section_index) = in_symbol.section {
            self.at_section_address(section_index, in_symbol.address as u32).find(|(_, symbol)| {
                symbol.kind == in_symbol.kind ||
                    // Replace auto symbols with real symbols
                    (symbol.kind == ObjSymbolKind::Unknown && is_auto_symbol(&symbol.name))
            })
        } else if self.obj_kind == ObjKind::Executable {
            // TODO hmmm
            self.iter_abs().find(|(_, symbol)| symbol.name == in_symbol.name)
        } else {
            bail!("ABS symbol in relocatable object: {:?}", in_symbol);
        };
        let target_symbol_idx = if let Some((symbol_idx, existing)) = opt {
            let size =
                if existing.size_known && in_symbol.size_known && existing.size != in_symbol.size {
                    // TODO fix and promote back to warning
                    log::debug!(
                        "Conflicting size for {}: was {:#X}, now {:#X}",
                        existing.name,
                        existing.size,
                        in_symbol.size
                    );
                    if replace {
                        in_symbol.size
                    } else {
                        existing.size
                    }
                } else if in_symbol.size_known {
                    in_symbol.size
                } else {
                    existing.size
                };
            if !replace {
                // Not replacing existing symbol, but update size
                if in_symbol.size_known && !existing.size_known {
                    self.replace(symbol_idx, ObjSymbol {
                        size: in_symbol.size,
                        size_known: true,
                        ..existing.clone()
                    })?;
                }
                return Ok(symbol_idx);
            }
            let new_symbol = ObjSymbol {
                name: in_symbol.name,
                demangled_name: in_symbol.demangled_name,
                address: in_symbol.address,
                section: in_symbol.section,
                size,
                size_known: existing.size_known || in_symbol.size != 0,
                flags: in_symbol.flags,
                kind: in_symbol.kind,
                align: in_symbol.align.or(existing.align),
                data_kind: match in_symbol.data_kind {
                    ObjDataKind::Unknown => existing.data_kind,
                    kind => kind,
                },
            };
            if existing != &new_symbol {
                log::debug!("Replacing {:?} with {:?}", existing, new_symbol);
                self.replace(symbol_idx, new_symbol)?;
            }
            symbol_idx
        } else {
            let target_symbol_idx = self.symbols.len();
            self.add_direct(ObjSymbol {
                name: in_symbol.name,
                demangled_name: in_symbol.demangled_name,
                address: in_symbol.address,
                section: in_symbol.section,
                size: in_symbol.size,
                size_known: in_symbol.size != 0,
                flags: in_symbol.flags,
                kind: in_symbol.kind,
                align: in_symbol.align,
                data_kind: in_symbol.data_kind,
            })?;
            target_symbol_idx
        };
        Ok(target_symbol_idx)
    }

    pub fn add_direct(&mut self, in_symbol: ObjSymbol) -> Result<SymbolIndex> {
        let symbol_idx = self.symbols.len();
        self.symbols_by_address.nested_push(in_symbol.address as u32, symbol_idx);
        if let Some(section_idx) = in_symbol.section {
            if section_idx >= self.symbols_by_section.len() {
                self.symbols_by_section.resize_with(section_idx + 1, BTreeMap::new);
            }
            self.symbols_by_section[section_idx].nested_push(in_symbol.address as u32, symbol_idx);
        } else {
            ensure!(
                in_symbol.address == 0
                    || in_symbol.flags.is_common()
                    || self.obj_kind == ObjKind::Executable,
                "ABS symbol in relocatable object"
            );
        }
        if !in_symbol.name.is_empty() {
            self.symbols_by_name.nested_push(in_symbol.name.clone(), symbol_idx);
        }
        self.symbols.push(in_symbol);
        Ok(symbol_idx)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ObjSymbol> { self.symbols.iter() }

    pub fn count(&self) -> usize { self.symbols.len() }

    pub fn at_section_address(
        &self,
        section_idx: usize,
        addr: u32,
    ) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        self.symbols_by_section
            .get(section_idx)
            .and_then(|v| v.get(&addr))
            .into_iter()
            .flatten()
            .map(move |&idx| (idx, &self.symbols[idx]))
    }

    pub fn kind_at_section_address(
        &self,
        section_idx: usize,
        addr: u32,
        kind: ObjSymbolKind,
    ) -> Result<Option<(SymbolIndex, &ObjSymbol)>> {
        self.at_section_address(section_idx, addr)
            .filter(|(_, sym)| sym.kind == kind)
            .at_most_one()
            .map_err(|_| anyhow!("Multiple symbols of kind {:?} at address {:#010X}", kind, addr))
    }

    // Iterate over all in address ascending order, excluding ABS symbols
    pub fn iter_ordered(&self) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        self.symbols_by_section
            .iter()
            .flat_map(|v| v.iter().map(|(_, v)| v))
            .flat_map(move |v| v.iter().map(move |u| (*u, &self.symbols[*u])))
    }

    // Iterate over all ABS symbols
    pub fn iter_abs(&self) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        debug_assert!(self.obj_kind == ObjKind::Executable);
        self.symbols_by_address
            .iter()
            .flat_map(|(_, v)| v.iter().map(|&u| (u, &self.symbols[u])))
            .filter(|(_, s)| s.section.is_none())
    }

    // Iterate over range in address ascending order, excluding ABS symbols
    pub fn for_section_range<R>(
        &self,
        section_index: usize,
        range: R,
    ) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)>
    where
        R: RangeBounds<u32> + Clone,
    {
        self.symbols_by_section
            .get(section_index)
            .into_iter()
            .flat_map(move |v| v.range(range.clone()))
            .flat_map(move |(_, v)| v.iter().map(move |u| (*u, &self.symbols[*u])))
    }

    pub fn indexes_for_range<R>(
        &self,
        range: R,
    ) -> impl DoubleEndedIterator<Item = (u32, &[SymbolIndex])>
    where
        R: RangeBounds<u32>,
    {
        // debug_assert!(self.obj_kind == ObjKind::Executable);
        self.symbols_by_address.range(range).map(|(k, v)| (*k, v.as_ref()))
    }

    pub fn for_section(
        &self,
        section_idx: usize,
    ) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        self.symbols_by_section
            .get(section_idx)
            .into_iter()
            .flat_map(|v| v.iter().map(|(_, v)| v))
            .flat_map(move |v| v.iter().map(move |u| (*u, &self.symbols[*u])))
    }

    pub fn for_name(
        &self,
        name: &str,
    ) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        self.symbols_by_name
            .get(name)
            .into_iter()
            .flat_map(move |v| v.iter().map(move |u| (*u, &self.symbols[*u])))
    }

    pub fn by_name(&self, name: &str) -> Result<Option<(SymbolIndex, &ObjSymbol)>> {
        let mut iter = self.for_name(name);
        let result = iter.next();
        if let Some((index, symbol)) = result {
            if let Some((other_index, other_symbol)) = iter.next() {
                bail!(
                    "Multiple symbols with name {}: {} {:?} {:#010X} and {} {:?} {:#010X}",
                    name,
                    index,
                    symbol.kind,
                    symbol.address,
                    other_index,
                    other_symbol.kind,
                    other_symbol.address
                );
            }
        }
        Ok(result)
    }

    pub fn by_kind(
        &self,
        kind: ObjSymbolKind,
    ) -> impl DoubleEndedIterator<Item = (SymbolIndex, &ObjSymbol)> {
        self.symbols.iter().enumerate().filter(move |(_, sym)| sym.kind == kind)
    }

    pub fn replace(&mut self, index: SymbolIndex, symbol: ObjSymbol) -> Result<()> {
        let symbol_ref = &mut self.symbols[index];
        ensure!(symbol_ref.address == symbol.address, "Can't modify address with replace_symbol");
        ensure!(symbol_ref.section == symbol.section, "Can't modify section with replace_symbol");
        if symbol_ref.name != symbol.name {
            if !symbol_ref.name.is_empty() {
                self.symbols_by_name.nested_remove(&symbol_ref.name, &index);
            }
            if !symbol.name.is_empty() {
                self.symbols_by_name.nested_push(symbol.name.clone(), index);
            }
        }
        *symbol_ref = symbol;
        Ok(())
    }

    // Try to find a previous sized symbol that encompasses the target
    pub fn for_relocation(
        &self,
        target_addr: SectionAddress,
        reloc_kind: ObjRelocKind,
    ) -> Result<Option<(SymbolIndex, &ObjSymbol)>> {
        // ensure!(self.obj_kind == ObjKind::Executable);
        let mut result = None;
        for (_addr, symbol_idxs) in self.indexes_for_range(..=target_addr.address).rev() {
            let symbols = symbol_idxs
                .iter()
                .map(|&idx| (idx, &self.symbols[idx]))
                .filter(|(_, sym)| {
                    (sym.section.is_none() || sym.section == Some(target_addr.section))
                        && sym.referenced_by(reloc_kind)
                })
                .collect_vec();
            let Some((symbol_idx, symbol)) = best_match_for_reloc(symbols, reloc_kind) else {
                continue;
            };
            if symbol.address == target_addr.address as u64 {
                result = Some((symbol_idx, symbol));
                break;
            }
            if symbol.size > 0 {
                if symbol.address + symbol.size > target_addr.address as u64 {
                    result = Some((symbol_idx, symbol));
                }
                break;
            }
        }
        Ok(result)
    }

    #[inline]
    pub fn flags(&mut self, idx: SymbolIndex) -> &mut ObjSymbolFlagSet {
        &mut self.symbols[idx].flags
    }
}

impl Index<SymbolIndex> for ObjSymbols {
    type Output = ObjSymbol;

    fn index(&self, index: usize) -> &Self::Output { &self.symbols[index] }
}

impl ObjSymbol {
    /// Whether this symbol can be referenced by the given relocation kind.
    pub fn referenced_by(&self, reloc_kind: ObjRelocKind) -> bool {
        if self.flags.is_relocation_ignore() {
            return false;
        }

        if is_linker_generated_label(&self.name) {
            // Linker generated labels will only be referenced by @ha/@h/@l relocations
            return matches!(
                reloc_kind,
                ObjRelocKind::PpcAddr16Ha | ObjRelocKind::PpcAddr16Hi | ObjRelocKind::PpcAddr16Lo
            );
        }

        match self.kind {
            ObjSymbolKind::Unknown => true,
            ObjSymbolKind::Function => !matches!(reloc_kind, ObjRelocKind::PpcEmbSda21),
            ObjSymbolKind::Object => {
                !matches!(reloc_kind, ObjRelocKind::PpcRel14 | ObjRelocKind::PpcRel24)
            }
            ObjSymbolKind::Section => {
                matches!(
                    reloc_kind,
                    ObjRelocKind::PpcAddr16Ha
                        | ObjRelocKind::PpcAddr16Hi
                        | ObjRelocKind::PpcAddr16Lo
                )
            }
        }
    }
}

pub fn best_match_for_reloc(
    mut symbols: Vec<(SymbolIndex, &ObjSymbol)>,
    reloc_kind: ObjRelocKind,
) -> Option<(SymbolIndex, &ObjSymbol)> {
    if symbols.len() == 1 {
        return symbols.into_iter().next();
    }
    symbols.sort_by_key(|&(_, symbol)| {
        let mut rank = match symbol.kind {
            ObjSymbolKind::Function | ObjSymbolKind::Object => match reloc_kind {
                ObjRelocKind::PpcAddr16Hi
                | ObjRelocKind::PpcAddr16Ha
                | ObjRelocKind::PpcAddr16Lo => 1,
                ObjRelocKind::Absolute
                | ObjRelocKind::PpcRel24
                | ObjRelocKind::PpcRel14
                | ObjRelocKind::PpcEmbSda21 => 2,
            },
            // Label
            ObjSymbolKind::Unknown => match reloc_kind {
                ObjRelocKind::PpcAddr16Hi
                | ObjRelocKind::PpcAddr16Ha
                | ObjRelocKind::PpcAddr16Lo
                    if !symbol.name.starts_with("..") =>
                {
                    3
                }
                _ => 1,
            },
            ObjSymbolKind::Section => -1,
        };
        if symbol.size > 0 {
            rank += 1;
        }
        -rank
    });
    symbols.into_iter().next()
}