use crate::graph::CrateId;
use crate::StructType;

use std::collections::BTreeMap;

use crate::ast::ItemVisibility;
use crate::hir::def_map::{CrateDefMap, LocalModuleId, ModuleId};

// Returns false if the given private function is being called from a non-child module, or
// if the given pub(crate) function is being called from another crate. Otherwise returns true.
pub fn can_reference_module_id(
    def_maps: &BTreeMap<CrateId, CrateDefMap>,
    importing_crate: CrateId,
    current_module: LocalModuleId,
    target_module: ModuleId,
    visibility: ItemVisibility,
) -> bool {
    // Note that if the target module is in a different crate from the current module then we will either
    // return true as the target module is public or return false as it is private without looking at the `CrateDefMap` in either case.
    let same_crate = target_module.krate == importing_crate;

    match visibility {
        ItemVisibility::Public => true,
        ItemVisibility::PublicCrate => same_crate,
        ItemVisibility::Private => {
            let target_crate_def_map = &def_maps[&target_module.krate];
            same_crate
                && (module_descendent_of_target(
                    target_crate_def_map,
                    target_module.local_id,
                    current_module,
                ) || module_is_parent_of_struct_module(
                    target_crate_def_map,
                    current_module,
                    target_module.local_id,
                ))
        }
    }
}

// Returns true if `current` is a (potentially nested) child module of `target`.
// This is also true if `current == target`.
pub(crate) fn module_descendent_of_target(
    def_map: &CrateDefMap,
    target: LocalModuleId,
    current: LocalModuleId,
) -> bool {
    if current == target {
        return true;
    }

    def_map.modules[current.0]
        .parent
        .map_or(false, |parent| module_descendent_of_target(def_map, target, parent))
}

/// Returns true if `target` is a struct and its parent is `current`.
fn module_is_parent_of_struct_module(
    def_map: &CrateDefMap,
    current: LocalModuleId,
    target: LocalModuleId,
) -> bool {
    let module_data = &def_map.modules[target.0];
    module_data.is_struct && module_data.parent == Some(current)
}

pub fn struct_field_is_visible(
    struct_type: &StructType,
    visibility: ItemVisibility,
    current_module_id: ModuleId,
    def_maps: &BTreeMap<CrateId, CrateDefMap>,
) -> bool {
    match visibility {
        ItemVisibility::Public => true,
        ItemVisibility::PublicCrate => {
            struct_type.id.parent_module_id(def_maps).krate == current_module_id.krate
        }
        ItemVisibility::Private => {
            let struct_parent_module_id = struct_type.id.parent_module_id(def_maps);
            if struct_parent_module_id.krate != current_module_id.krate {
                return false;
            }

            if struct_parent_module_id.local_id == current_module_id.local_id {
                return true;
            }

            let def_map = &def_maps[&current_module_id.krate];
            module_descendent_of_target(
                def_map,
                struct_parent_module_id.local_id,
                current_module_id.local_id,
            )
        }
    }
}