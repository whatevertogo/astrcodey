# Memory Extension: Project-Scoped Storage

**Date**: 2026-05-22
**Version**: v0.1.2+

## Summary

The Memory extension now stores data on a **per-project basis** instead of using a global storage location. Each project has its own isolated memory store.

## Changes

### Before
```
~/.astrcode/extensions_data/astrcode.memory/
├── MEMORY.md
├── contexts/
└── processed_sessions.json
```

### After
```
~/.astrcode/projects/
├── D-work-project-a/
│   └── extension_data/
│       └── astrcode.memory/
│           ├── MEMORY.md
│           ├── contexts/
│           └── processed_sessions.json
└── D-work-project-b/
    └── extension_data/
        └── astrcode.memory/
            ├── MEMORY.md
            ├── contexts/
            └── processed_sessions.json
```

## Benefits

- **Project isolation**: Each project maintains its own memory, preventing cross-project contamination
- **Better organization**: Project-specific memories are stored alongside other project data
- **Easier cleanup**: Deleting a project's memory is as simple as removing its project directory

## Migration

**No automatic migration is provided** for existing global memories. If you have existing memories in the old global location that you want to preserve:

1. Copy `~/.astrcode/extensions_data/astrcode.memory/` to the relevant project directory:
   ```bash
   cp -r ~/.astrcode/extensions_data/astrcode.memory/ \
         ~/.astrcode/projects/<project_key>/extension_data/astrcode.memory/
   ```

2. The project key is derived from the project path (e.g., `/home/user/project` → `home-user-project`)

## Implementation Details

- Added `MemoryStorePool` to manage multiple project-scoped stores
- All handlers now use `working_dir` to determine which project's store to access
- Stores are created on-demand and cached for performance

## Files Modified

- `crates/astrcode-extension-memory/src/store.rs` - Added `MemoryStorePool`
- `crates/astrcode-extension-memory/src/handlers.rs` - Updated all handlers to use store pool
- `crates/astrcode-extension-memory/src/lib.rs` - Updated extension initialization
- `docs/configuration.md` - Updated documentation
- `docs/configuration_cn.md` - Updated Chinese documentation
