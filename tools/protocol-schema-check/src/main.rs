#![cfg_attr(enable_const_type_id, feature(const_type_id))]
#![allow(unused_imports)]

/// Tool which is able to iterate over all structs and check their hashes.
/// Iteration is done by `ProtocolSchemaInfo`s generated by `ProtocolSchema`
/// macro.

/// Needed because otherwise tool doesn't notice `ProtocolSchemaInfo`s from
/// other crates.
use near_chain::*;
use near_crypto::*;
use near_epoch_manager::*;
use near_jsonrpc_primitives::errors::*;
use near_network::*;
use near_parameters::*;
use near_primitives::*;
use near_store::*;
use near_vm_runner::*;

use near_epoch_manager::types::EpochInfoAggregator;
use near_schema_checker_lib::{FieldName, FieldTypeInfo, ProtocolSchema, ProtocolSchemaInfo};
use near_stable_hasher::StableHasher;
use std::any::TypeId;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

fn compute_hash(
    info: &ProtocolSchemaInfo,
    structs: &BTreeMap<TypeId, &'static ProtocolSchemaInfo>,
    types_in_compute: &mut HashSet<TypeId>,
) -> u32 {
    let type_id = info.type_id();
    if types_in_compute.contains(&type_id) {
        return 0;
    }
    types_in_compute.insert(type_id);

    let mut hasher = StableHasher::new();
    match info {
        ProtocolSchemaInfo::Struct { name, type_id: _, fields } => {
            "struct".hash(&mut hasher);
            name.hash(&mut hasher);
            compute_fields_hash(fields, structs, types_in_compute, &mut hasher);
        }
        ProtocolSchemaInfo::Enum { name, type_id: _, variants } => {
            "enum".hash(&mut hasher);
            name.hash(&mut hasher);
            for (variant_name, variant_fields) in *variants {
                variant_name.hash(&mut hasher);
                if let Some(fields) = variant_fields {
                    compute_fields_hash(fields, structs, types_in_compute, &mut hasher);
                }
            }
        }
    }

    types_in_compute.remove(&type_id);

    hasher.finish() as u32
}

fn compute_fields_hash(
    fields: &'static [(FieldName, FieldTypeInfo)],
    structs: &BTreeMap<TypeId, &'static ProtocolSchemaInfo>,
    types_in_compute: &mut HashSet<TypeId>,
    hasher: &mut StableHasher,
) {
    for (field_name, (type_name, generic_params)) in fields {
        field_name.hash(hasher);
        type_name.hash(hasher);
        for &param_type_id in generic_params.iter() {
            compute_type_hash(param_type_id, structs, types_in_compute, hasher);
        }
    }
}

fn compute_type_hash(
    type_id: TypeId,
    structs: &BTreeMap<TypeId, &'static ProtocolSchemaInfo>,
    types_in_compute: &mut HashSet<TypeId>,
    hasher: &mut StableHasher,
) {
    if let Some(nested_info) = structs.get(&type_id) {
        compute_hash(nested_info, structs, types_in_compute).hash(hasher);
    } else {
        // Unsupported type. Always assume that hash is 0 because we cannot
        // compute nontrivial deterministic hash in such cases.
        0.hash(hasher);
    }
}

const PROTOCOL_SCHEMA_FILE: &str = "protocol_schema.toml";

fn main() {
    #[cfg(enable_const_type_id)]
    {
        // For some reason, `EpochInfoAggregator` is not picked up by `inventory`
        // crate at all. In addition to that, `Latest*` structs are not picked up
        // on macos. This is a workaround around that. It is enough to put only
        // `LatestKnown` and `ServerError` here but I don't know why as well.
        // The issue may be related to the large size of crates. Other workaround
        // is to move these types to smaller crates.
        // TODO (#11755): find the reason and remove this workaround.
        LatestKnown::ensure_registration();
        LatestWitnessesInfo::ensure_registration();
        EpochInfoAggregator::ensure_registration();
        ServerError::ensure_registration();
    }

    let source_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("res").join(PROTOCOL_SCHEMA_FILE);
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("./target"));
    let target_path = target_dir.join(PROTOCOL_SCHEMA_FILE);

    let stored_hashes: BTreeMap<String, u32> = if source_path.exists() {
        toml::from_str(&fs::read_to_string(&source_path).unwrap_or_else(|_| "".to_string()))
            .unwrap()
    } else {
        BTreeMap::new()
    };

    let structs: BTreeMap<TypeId, &'static ProtocolSchemaInfo> =
        inventory::iter::<ProtocolSchemaInfo>
            .into_iter()
            .map(|info| (info.type_id(), info))
            .collect();

    println!("Loaded {} structs", structs.len());

    let mut current_hashes: BTreeMap<String, u32> = Default::default();
    for info in inventory::iter::<ProtocolSchemaInfo> {
        let mut types_in_compute: HashSet<TypeId> = Default::default();
        let hash = compute_hash(info, &structs, &mut types_in_compute);
        current_hashes.insert(info.type_name().to_string(), hash);
    }

    let mut has_changes = false;
    for (name, hash) in &current_hashes {
        match stored_hashes.get(name) {
            Some(stored_hash) if stored_hash != hash => {
                println!("Hash mismatch for {}: stored {}, current {}", name, stored_hash, hash);
                has_changes = true;
            }
            None => {
                println!("New struct: {} with hash {}", name, hash);
                has_changes = true;
            }
            _ => {}
        }
    }

    let current_keys: HashSet<_> = current_hashes.keys().collect();
    let stored_keys: HashSet<_> = stored_hashes.keys().collect();
    for removed in stored_keys.difference(&current_keys) {
        println!("Struct removed: {}", removed);
        has_changes = true;
    }

    if has_changes {
        fs::write(&target_path, toml::to_string_pretty(&current_hashes).unwrap()).unwrap();
        println!("New TOML file written to: {}", target_path.display());
        println!(
            "Please review the changes and copy the file to {} if they are correct.",
            PROTOCOL_SCHEMA_FILE
        );
        std::process::exit(1);
    } else {
        println!("No changes detected in protocol structs");
    }
}

#[cfg(all(test, enable_const_type_id))]
mod tests {
    use super::*;
    use near_schema_checker_lib::ProtocolSchema;
    use std::collections::HashMap;

    fn do_compute_type_hash(
        ty: TypeId,
        structs: &BTreeMap<TypeId, &'static ProtocolSchemaInfo>,
    ) -> u32 {
        let mut hasher = StableHasher::new();
        let mut types_in_compute: HashSet<TypeId> = Default::default();
        compute_type_hash(ty, structs, &mut types_in_compute, &mut hasher);
        hasher.finish() as u32
    }

    fn check_types(
        ty: TypeId,
        other_ty: TypeId,
        expect_equal: bool,
        structs: &BTreeMap<TypeId, &'static ProtocolSchemaInfo>,
    ) {
        let hash = do_compute_type_hash(ty, structs);
        let other_hash = do_compute_type_hash(other_ty, structs);
        assert_eq!(hash == other_hash, expect_equal);
    }

    fn collect_structs() -> BTreeMap<TypeId, &'static ProtocolSchemaInfo> {
        inventory::iter::<ProtocolSchemaInfo>
            .into_iter()
            .map(|info| (info.type_id(), info))
            .collect()
    }

    /// Helper types for tests.
    type TestU64 = u64;
    #[derive(ProtocolSchema)]
    #[allow(unused)]
    struct TestStruct {
        a: u64,
        b: String,
    }
    use TestStruct as TestStruct2;

    /// Checks that structs with same names and underlying structure have the
    /// same hash, even if used with different aliases.
    #[test]
    fn test_identical() {
        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct TestStruct {
            a: u64,
            b: String,
        }

        check_types(
            TypeId::of::<TestStruct>(),
            TypeId::of::<TestStruct2>(),
            true,
            &collect_structs(),
        );
    }

    /// Checks that if identical structs have different field names, hashes are
    /// different.
    #[test]
    fn test_different_field_names() {
        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct TestStruct {
            a: u64,
            c: String,
        }

        check_types(
            TypeId::of::<TestStruct>(),
            TypeId::of::<TestStruct2>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that if identical structs have different type names, hashes are
    /// different.
    #[test]
    fn test_different_type_names() {
        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct TestStruct {
            a: TestU64,
            b: String,
        }

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct TestStruct2 {
            a: u64,
            b: String,
        }

        check_types(
            TypeId::of::<TestStruct>(),
            TypeId::of::<TestStruct2>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that struct and enum have different hashes.
    #[test]
    fn test_different_struct_enum() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            pub struct Empty;
        }
        use inner::Empty as EmptyStruct;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        enum Empty {}

        check_types(TypeId::of::<Empty>(), TypeId::of::<EmptyStruct>(), false, &collect_structs());
    }

    /// Checks that hashes can differentiate integers.
    #[test]
    fn test_different_integers() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Unsigned {
                a: u32,
            }
        }
        use inner::Unsigned as ShortUnsigned;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Unsigned {
            a: u64,
        }

        check_types(
            TypeId::of::<Unsigned>(),
            TypeId::of::<ShortUnsigned>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that hashes can differentiate containers.
    #[test]
    fn test_different_containers() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Container {
                a: Vec<u32>,
            }
        }
        use inner::Container as VecContainer;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Container {
            a: HashMap<u32, u32>,
        }

        check_types(
            TypeId::of::<Container>(),
            TypeId::of::<VecContainer>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that hashes can differentiate generics in containers.
    #[test]
    fn test_different_container_generics() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Container {
                a: Vec<Vec<Vec<u32>>>,
            }
        }
        use inner::Container as VecContainer;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Container {
            a: Vec<Vec<Vec<i32>>>,
        }

        check_types(
            TypeId::of::<Container>(),
            TypeId::of::<VecContainer>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that hashes can differentiate one of generics in containers.
    #[test]
    fn test_different_container_two_generics() {
        mod inner {
            use super::*;

            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Container {
                a: HashMap<u32, u16>,
            }
        }
        use inner::Container as MapContainer;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Container {
            a: HashMap<u32, u32>,
        }

        check_types(
            TypeId::of::<Container>(),
            TypeId::of::<MapContainer>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that hashes can differentiate nested containers.
    #[test]
    fn test_nested_containers_different_types() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Container {
                a: Vec<Vec<u32>>,
            }
        }
        use inner::Container as VecContainer;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Container {
            a: Vec<Vec<i32>>,
        }

        check_types(
            TypeId::of::<Container>(),
            TypeId::of::<VecContainer>(),
            false,
            &collect_structs(),
        );
    }

    /// Checks that if nested containers differ, this is not caught by hash
    /// check.
    /// Added to indicate limitations of implementation.
    #[test]
    fn test_nested_containers_different_containers_unsupported() {
        mod inner {
            #[derive(super::ProtocolSchema)]
            #[allow(unused)]
            pub struct Container {
                a: Vec<Vec<u32>>,
            }
        }
        use inner::Container as VecContainer;

        #[derive(ProtocolSchema)]
        #[allow(unused)]
        struct Container {
            a: Vec<HashSet<u32>>,
        }

        check_types(
            TypeId::of::<Container>(),
            TypeId::of::<VecContainer>(),
            true,
            &collect_structs(),
        );
    }
}
