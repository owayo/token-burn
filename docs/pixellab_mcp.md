# PixelLab MCP Tools - AI Assistant Guide

> Generate pixel art assets directly from your AI coding assistant using the Model Context Protocol (MCP)

You have access to PixelLab's MCP tools for creating game-ready pixel art. This guide explains what tools are available and how to use them effectively.

## ⚠️ IMPORTANT: These are MCP tools, not REST endpoints

- If you see PixelLab tools available (like `create_character`, `animate_character`), use them directly
- If you don't see these tools, tell the user MCP isn't configured - don't try curl or v2 API
- Tool names may be prefixed (`mcp__pixellab__create_character`) or bare (`create_character`) depending on client
- Download URLs like `/mcp/characters/{id}/download` are the only direct HTTP endpoints

## What is PixelLab MCP?

PixelLab MCP (also called "Vibe Coding") lets you generate pixel art characters, animations, and tilesets while coding. The tools are non-blocking - they return job IDs immediately and process in the background (typically 2-5 minutes).

## Other PixelLab Interfaces

If users mention these, they're using different PixelLab interfaces:
- **Web interfaces**: Character Creator, Map Workshop, Simple Creator (visual tools at pixellab.ai)
- **Editor plugins**: Aseprite extension, Pixelorama integration
- **API v1**: Legacy REST endpoints (deprecated)
- **API v2**: Modern REST API for programmatic usage
  - Documentation: https://api.pixellab.ai/v2/llms.txt
  - You can use these endpoints directly via HTTP requests when writing code
  - Same functionality as MCP but through REST calls
  - Useful for: batch processing, custom integrations, live in-game asset generation
  - Also useful when you need endpoints not available as MCP tools (use at own risk - MCP tools are made for easy AI usage)

Note: Characters and tilesets created via MCP will appear in Character Creator and Map Workshop web interfaces respectively (if using the same account).

## MCP/Vibe Coding Setup

1. Get your API token at https://api.pixellab.ai/mcp
2. Configure your AI assistant (Cursor, Claude Code, VS Code, etc.)
3. Start generating game assets while coding

### MCP Configuration
```json
{
  "mcpServers": {
    "pixellab": {
      "url": "https://api.pixellab.ai/mcp",
      "transport": "http",
      "headers": {
        "Authorization": "Bearer YOUR_API_TOKEN"
      }
    }
  }
}
```

## Key Concepts

### Non-Blocking Operations
All creation tools return immediately with job IDs:
- Submit request → Get job ID instantly
- Process runs in background (2-5 minutes)
- Check status with corresponding `get_*` tool
- Download when ready (UUID serves as access key)
- **No authentication for downloads**: Share download links freely - UUID acts as the key

### Workflow Pattern
```python
# 1. Create (returns immediately)
result = create_character(description='wizard', n_directions=8)
character_id = result.character_id

# 2. Queue animations immediately (no waiting!)
animate_character(character_id, 'walk')
animate_character(character_id, 'idle')

# 3. Check status later
status = get_character(character_id)
```

### Connected Tilesets
Create seamless terrain transitions by chaining tilesets:
```python
# First tileset returns base_tile_ids immediately
t1 = create_topdown_tileset('ocean', 'beach')

# Chain next tileset using base tile ID (no waiting!)
t2 = create_topdown_tileset('beach', 'grass', lower_base_tile_id=t1.beach_base_id)
```

## Available Tools

### Character & Animation Tools

**💡 Tips:**
- Characters are stored permanently and can be reused
- Animations can be queued immediately after character creation
- 4 directions: south, west, east, north
- 8 directions: adds south-east, north-east, north-west, south-west
- Canvas size is total area; character will be ~60% of canvas height

#### `create_character`
Queue a character creation job with 4 or 8 directional views.

**Examples:**
```python
# Humanoid character (default)
create_character(
    description='brave knight with shining armor',
    n_directions=8,
    size=48,
    proportions='{"type": "preset", "name": "heroic"}'
)

# Quadruped character
create_character(
    description='orange tabby cat',
    body_type='quadruped',
    template='cat',  # bear, cat, dog, horse, lion
    n_directions=8,
    size=48
)
```

**Parameters:**
- `description`: str (optional) [default: PydanticUndefined]
- `name`: Optional (optional) [default: None]
- `body_type`: Literal (optional) [default: humanoid] - 'humanoid' for people/robots (default), 'quadruped' for animals (requires template)
- `template`: Optional (optional) [default: None] - Required for quadrupeds: bear, cat, dog, horse, lion
- `n_directions`: Literal (optional) [default: 8] - Use 8 for full rotation, 4 for cardinal directions
- `proportions`: Optional (optional) [default: {"type": "preset", "name": "default"}] - Presets: default, chibi, cartoon, stylized, realistic_male, realistic_female, heroic (humanoid only)
- `size`: int (optional) [default: 48] - Canvas size in pixels (character ~60% of height)
- `outline`: Optional (optional) [default: single color black outline]
- `shading`: Optional (optional) [default: basic shading]
- `detail`: Optional (optional) [default: medium detail]
- `ai_freedom`: float (optional) [default: 750]
- `view`: Literal (optional) [default: low top-down]

#### `animate_character`
Queue animation jobs for an existing character (humanoid or quadruped).

**Example:**
```python
animate_character(
    character_id='uuid-from-create',
    template_animation_id='walking',  # Check tool description for full list
    action_description='walking proudly'  # optional customization
)
```

**Parameters:**
- `character_id`: str (optional) [default: PydanticUndefined]
- `template_animation_id`: str (optional) [default: PydanticUndefined]
- `action_description`: Optional (optional) [default: None]
- `animation_name`: Optional (optional) [default: None]

#### `get_character`
Get complete character information including rotations, animations, and download link.
    
    Returns:
    - Character details and metadata
    - All rotation image URLs
    - List of animations with their status
    - Pending jobs for this character
    - ZIP download URL
    - Optional preview image

**Parameters:**
- `character_id`: str (optional) [default: PydanticUndefined]
- `include_preview`: bool (optional) [default: True]

#### `list_characters`
List all your created characters.

**Parameters:**
- `limit`: int (optional) [default: 10]
- `offset`: int (optional) [default: 0]
- `tags`: str | None (optional) [default: None]

#### `delete_character`
Delete a character and all its associated data.

**Parameters:**
- `character_id`: str (optional) [default: PydanticUndefined]

### Top-Down Tileset Tools

**💡 Wang Tileset System:**
- Creates 16 tiles covering all corner combinations
- Perfect for seamless terrain transitions
- Use base_tile_ids to chain multiple tilesets
- tile_size: typically 16x16 or 32x32 pixels
- transition_size: 0=sharp, 0.25=medium, 0.5=wide blend

#### `create_topdown_tileset`
Generate a Wang tileset for top-down game maps with corner-based autotiling.

**Example (chained tilesets):**
```python
# Ocean → Beach → Grass → Forest
t1 = create_topdown_tileset('ocean water', 'sandy beach')
t2 = create_topdown_tileset('sandy beach', 'green grass',
                           lower_base_tile_id=t1.beach_base_id)
t3 = create_topdown_tileset('green grass', 'dense forest',
                           lower_base_tile_id=t2.grass_base_id)
```

**Parameters:**
- `lower_description`: str (optional) [default: PydanticUndefined]
- `upper_description`: str (optional) [default: PydanticUndefined]
- `transition_size`: float (optional) [default: 0.0] - 0=sharp edge, 0.25=medium blend, 0.5=wide transition
- `transition_description`: Optional (optional) [default: None]
- `tile_size`: Dict (optional) [default: {'width': 16, 'height': 16}]
- `outline`: Optional (optional) [default: None]
- `shading`: Optional (optional) [default: None]
- `detail`: Optional (optional) [default: None]
- `view`: Literal (optional) [default: high top-down] - 'high top-down' for RTS, 'low top-down' for RPG
- `tile_strength`: float (optional) [default: 1.0]
- `lower_base_tile_id`: Optional (optional) [default: None] - Use base_tile_id from previous tileset for continuity
- `upper_base_tile_id`: Optional (optional) [default: None]
- `tileset_adherence`: float (optional) [default: 100.0]
- `tileset_adherence_freedom`: float (optional) [default: 500.0]
- `text_guidance_scale`: float (optional) [default: 8.0]

#### `get_topdown_tileset`
Retrieve a topdown tileset by ID.

**Parameters:**
- `tileset_id`: str (optional) [default: PydanticUndefined]

#### `list_topdown_tilesets`
List all tilesets created by the authenticated user.

**Parameters:**
- `limit`: int (optional) [default: 10]
- `offset`: int (optional) [default: 0]

#### `delete_topdown_tileset`
Delete a tileset by ID.
    
    Args:
        tileset_id: The UUID of the tileset to delete
        
    Returns:
        Success or error message

**Parameters:**
- `tileset_id`: str (optional) [default: PydanticUndefined]

### Sidescroller Tileset Tools

**💡 2D Platformer Tips:**
- Designed for side-view perspective (not top-down)
- Creates 16 tiles with transparent backgrounds
- Platform tiles have flat surfaces for gameplay
- Use `transition_description` for decorative top layers
- Chain tilesets using `base_tile_id` for consistency

#### `create_sidescroller_tileset`
Generate a sidescroller tileset for 2D platformer games with side-view perspective.

**Example (platform variety):**
```python
# Stone → Wood → Metal platforms
stone = create_sidescroller_tileset(
    lower_description='stone brick',
    transition_description='moss and vines'
)
wood = create_sidescroller_tileset(
    lower_description='wooden planks',
    transition_description='grass',
    base_tile_id=stone.base_tile_id
)
```

**Parameters:**
- `lower_description`: str (optional) [default: PydanticUndefined] - Platform material (stone, wood, metal, ice, etc.)
- `transition_description`: str (optional) [default: PydanticUndefined] - Top decoration (grass, snow, moss, rust, etc.)
- `transition_size`: float (optional) [default: 0.0] - 0=no top layer, 0.25=light decoration, 0.5=heavy coverage
- `tile_size`: Dict (optional) [default: {'width': 16, 'height': 16}]
- `outline`: Optional (optional) [default: None]
- `shading`: Optional (optional) [default: None]
- `detail`: Optional (optional) [default: None]
- `tile_strength`: float (optional) [default: 1.0]
- `base_tile_id`: Optional (optional) [default: None] - From previous tileset for visual consistency
- `tileset_adherence`: float (optional) [default: 100.0]
- `tileset_adherence_freedom`: float (optional) [default: 500.0]
- `text_guidance_scale`: float (optional) [default: 8.0]
- `seed`: Optional (optional) [default: None]

#### `get_sidescroller_tileset`
Get a sidescroller tileset by ID, including generation status and download information.

**Parameters:**
- `tileset_id`: str (optional) [default: PydanticUndefined]
- `include_example_map`: bool (optional) [default: True] - Shows how tiles work in a platformer level

#### `list_sidescroller_tilesets`
List all sidescroller tilesets created by the authenticated user.

**Parameters:**
- `limit`: int (optional) [default: 20]
- `offset`: int (optional) [default: 0]

#### `delete_sidescroller_tileset`
Delete a sidescroller tileset by ID.
    
    Args:
        tileset_id: The UUID of the sidescroller tileset to delete
        
    Returns:
        Success or error message

**Parameters:**
- `tileset_id`: str (optional) [default: PydanticUndefined]

### Isometric Tile Tools

**💡 Isometric Design Tips:**
- Creates individual 3D-looking tiles for game assets
- Sizes above 24px produce better quality (32px recommended)
- `tile_shape` controls thickness: thin (~10%), thick (~25%), block (~50%)
- Perfect for blocks, items, terrain pieces, buildings
- Use consistent settings across tiles for cohesive look

#### `create_isometric_tile`
Create an isometric tile with pixel art style.

**Examples:**
```python
# Terrain tiles
grass = create_isometric_tile('grass on top of dirt', size=32)
stone = create_isometric_tile('stone brick wall with moss', size=32)

# Game objects
chest = create_isometric_tile(
    description='wooden treasure chest with gold trim',
    tile_shape='block',  # Full height for objects
    detail='highly detailed'
)
```

**Parameters:**
- `description`: str (optional) [default: PydanticUndefined]
- `size`: int (optional) [default: 32] - 32px recommended for best quality
- `tile_shape`: Literal (optional) [default: block] - thin (floors), thick (platforms), block (cubes/objects)
- `outline`: Optional (optional) [default: lineless] - 'lineless' for modern look, 'single color' for retro
- `shading`: Optional (optional) [default: basic shading]
- `detail`: Optional (optional) [default: medium detail] - Higher detail works better with larger sizes
- `text_guidance_scale`: float (optional) [default: 8.0] - Higher values follow description more closely
- `seed`: Optional (optional) [default: None] - Use same seed for consistent style across tiles

#### `get_isometric_tile`
Retrieve an isometric tile by ID. Returns tile data if completed, or status information if still processing.
    
    The tile will include:
    - Base64 PNG image data
    - Metadata about generation parameters
    - Download URL for the tile image
    
    Check the 'status' field:
    - 'completed': Tile is ready with full data
    - 'processing': Still generating (check 'eta_seconds')
    - 'failed': Generation failed (see 'error' message)
    - 'not_found': Invalid tile ID

**Parameters:**
- `tile_id`: str (optional) [default: PydanticUndefined]

#### `list_isometric_tiles`
List all your created isometric tiles.
    
    Returns a paginated list of tiles with basic information.
    Use get_isometric_tile for detailed information about a specific tile.
    
    Tiles are sorted by creation date (newest first).

**Parameters:**
- `limit`: int (optional) [default: 10]
- `offset`: int (optional) [default: 0]

#### `delete_isometric_tile`
Delete an isometric tile by ID. Only the owner can delete their own tiles.
    
    This action is permanent and cannot be undone.

**Parameters:**
- `tile_id`: str (optional) [default: PydanticUndefined]

### Map Object Tools

#### `create_map_object`
Create a pixel art object with transparent background for use in game maps.

**Parameters:**
- `description`: str (optional) [default: PydanticUndefined]
- `width`: Optional (optional) [default: None]
- `height`: Optional (optional) [default: None]
- `view`: Literal (optional) [default: high top-down]
- `outline`: Optional (optional) [default: single color outline]
- `shading`: Optional (optional) [default: medium shading]
- `detail`: Optional (optional) [default: medium detail]
- `background_image`: Optional (optional) [default: None]
- `inpainting`: Optional (optional) [default: None]

#### `get_map_object`
Get map object information and status.

**Parameters:**
- `object_id`: str (optional) [default: PydanticUndefined]

#### `create_tiles_pro`
Create pixel art tiles (pro).

**Parameters:**
- `description`: str (optional) [default: PydanticUndefined]
- `tile_type`: Literal (optional) [default: isometric]
- `tile_size`: int (optional) [default: 32]
- `tile_height`: Optional (optional) [default: None]
- `n_tiles`: Optional (optional) [default: None]
- `tile_view`: Literal (optional) [default: low top-down]
- `tile_view_angle`: Optional (optional) [default: None]
- `tile_depth_ratio`: Optional (optional) [default: None]
- `seed`: Optional (optional) [default: None]
- `style_images`: Optional (optional) [default: None]
- `style_options`: Optional (optional) [default: None]

#### `get_tiles_pro`
Retrieve tiles pro by ID. Returns tile data if completed, or status if processing.

**Parameters:**
- `tile_id`: str (optional) [default: PydanticUndefined]

#### `list_tiles_pro`
List all your created tiles (pro).

**Parameters:**
- `limit`: int (optional) [default: 10]
- `offset`: int (optional) [default: 0]

#### `delete_tiles_pro`
Delete tiles pro by ID. Only the owner can delete their own tiles.

**Parameters:**
- `tile_id`: str (optional) [default: PydanticUndefined]

## Available Resources

MCP also provides documentation resources:

- `pixellab://docs/python/sidescroller-tilesets`
  Quick Python implementation guide for PixelLab sidescroller tilesets
- `pixellab://docs/godot/sidescroller-tilesets`
  Complete Godot 4.x sidescroller tileset implementation guide with PixelLab MCP integration and headless GDScript converter
- `pixellab://docs/unity/isometric-tilemaps-2d`
  Complete Unity 2D isometric tilemap implementation guide with elevation support
- `pixellab://docs/godot/isometric-tiles`
  Complete Godot 4.x isometric tiles guide with PixelLab MCP integration and proper TileSet configuration
- `pixellab://docs/godot/wang-tilesets`
  Complete Godot 4.x Wang tileset implementation guide with PixelLab MCP integration and headless GDScript converter
- `pixellab://docs/python/wang-tilesets`
  Quick Python implementation guide for PixelLab Wang tilesets
- `pixellab://docs/overview`
  Complete PixelLab platform overview including all interfaces,
MCP tools, and integration methods.

## Tool Response Format

All tools return status indicators:
- ✅ Success - Operation completed
- ⏳ Processing - Background job running
- ❌ Error - Operation failed

## Background Jobs

Creation tools return immediately with job IDs.
Use the corresponding `get_*` tool to check status.

## Support & Resources

- Setup Guide: https://pixellab.ai/vibe-coding
- Discord Community: https://discord.gg/pBeyTBF8T7
- API v2 Documentation: https://api.pixellab.ai/v2/llms.txt

## About Vibe Coding

Vibe Coding transforms game development by enabling AI assistants to generate production-ready pixel art assets on-demand while writing game code. Build complete games faster with AI as your art department and coding partner.

---
*Generated: 2026-03-03 09:42 - This documentation is auto-generated from FastMCP tool definitions.*
