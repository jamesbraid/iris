//! The selector tables: for each command word a port can open, how many
//! argument words follow. Counts are those of the senders read.
//!
//! Ported from hw/misc/sgi-kona-ports.c in jamesbraid/qemu-sgi-origami.

use super::defs::*;
use super::frame::Selector;

/// The kernel FIFO push port: the DMA packet, the list-write record and the pool
/// registration, then the display-mode and select-feed selectors.
pub(super) const DMA_FIFO: &[Selector] = &[
    Selector::new(START_INPUT_DMA, 6),
    Selector::new(COPY_LIST_BY_INPUT_DMA_THEN_LIST_DMA, 2),
    Selector::new(DESCRIBE_DMA_PAGE_POOL, 3),
    Selector::new(DISPLAY_MODE_BYTE, 3),
    Selector::new(START_SELECT_FEEDBACK_RETURN_DMA, 0),
    Selector::new(FINISH_SELECT_FEEDBACK_WITH_EVENT_CODE, 0),
];

/// The textport's commands, which follow a sync word.
pub(super) const TEXTPORT: &[Selector] = &[
    Selector::new(TEXTPORT_UNMODELLED_1, 1),
    Selector::new(TEXTPORT_SET_COLOUR, 1),
    Selector::new(TEXTPORT_LOAD_COLOUR_MAP, 4),
    Selector::new(TEXTPORT_SET_RASTER_POSITION, 2),
    Selector::new(TEXTPORT_FILL_BOX, 4),
    Selector::new(TEXTPORT_PLOT_POINT, 2),
    Selector::new(TEXTPORT_UNMODELLED_7, 2),
    Selector::new(TEXTPORT_UNMODELLED_8, 2),
    Selector::new(TEXTPORT_SYNC_PROBE, 1),
    Selector::new(TEXTPORT_GRAPHICS_INFO, 1),
];

/// The command page's tokens: kernel, graphics-process and X server commands.
pub(super) const KERNEL: &[Selector] = &[
    Selector::new(PIPE_SET_UP, 3),
    Selector::new(SET_UP_OUTPUT_DMA_FOR_HOST_READ, 3),
    Selector::new(CONTEXT_INITIALISE, 3),
    Selector::new(CONTEXT_CLEAN_UP_WITH_EVENT_CODE, 1),
    Selector::new(RAISE_HOST_INTERRUPT_WITH_EVENT_CODE, 1),
    Selector::new(QUERY_BOARD_CONFIGURATION_BLOCK, 1),
    Selector::new(EEPROM_OPERATION, 1),
    Selector::new(DISPLAY_MODE_BYTE, 3),
    Selector::new(CONTEXT_DEACTIVATE, 1),
    Selector::new(CONTEXT_IMAGE_LOAD, 4),
    Selector::new(WINDOW_CLIP_SETUP, 11),
    Selector::new(WINDOW_READ_CLIP_SET_UP, 3),
    Selector::new(DISPLAY_BANK_SET_UP, 2),
    Selector::new(DESCRIBE_DMA_PAGE_POOL, 3),
    Selector::new(START_INPUT_DMA, 6),
    Selector::new(COPY_LIST_BY_INPUT_DMA_THEN_LIST_DMA, 2),
    Selector::new(START_SELECT_FEEDBACK_RETURN_DMA, 6),
    Selector::new(FINISH_SELECT_FEEDBACK_WITH_EVENT_CODE, 2),
    Selector::new(EXECUTE_LIST_BY_LIST_DMA, 2),
    Selector::new(EXECUTE_LIST_BY_INPUT_DMA, 2),
    Selector::new(EXECUTE_LIST_BY_LIST_DMA_REPEATED, 1),
    Selector::new(EXECUTE_LIST_BY_INPUT_DMA_REPEATED, 1),
    Selector::new(COPY_USER_LIST_BY_LIST_DMA, 4),
    Selector::new(MOVE_LIST_BY_LIST_DMA, 3),
    Selector::new(PUBLISH_RETRACE_SEQUENCE_NUMBER, 1),
    Selector::new(PIPE_MEMORY_RESERVE, 4),
    Selector::new(PIPE_MEMORY_COPY_DOWN, 4),
    Selector::new(PIPE_MEMORY_SWAP_OUT, 8),
    Selector::new(PIPE_MEMORY_SWAP_IN, 6),
    Selector::new(PIPE_MEMORY_SUSPEND_WITH_EVENT_CODE, 1),
    Selector::new(PIPE_MEMORY_RESUME, 0),
    Selector::new(DISTRIBUTOR_NO_OP_PADDING_WORD, 1),
    Selector::new(WINDOW_SYSTEM_CONTEXT_INITIALISE, 3),
    Selector::new(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, 13),
    Selector::new(WINDOW_SYSTEM_DRAWABLE_ORIGIN_AND_SIZE_LOAD, 4),
    Selector::new(WINDOW_SYSTEM_FOUR_WORD_DATA_PRELOAD_BYTE_FORM, 4),
    Selector::new(WINDOW_SYSTEM_CLIP_LOAD, 5),
    Selector::new(WINDOW_SYSTEM_SCISSOR_LOAD, 4),
    Selector::new(LOAD_COLOUR_MAP_ENTRIES, 2),
    Selector::new(LOAD_GAMMA_TABLE_ENTRIES, 2),
    Selector::new(LOAD_CURSOR_IMAGE_BLOCK, 1),
    Selector::new(LOAD_CURSOR_COLOUR_WORDS, 1),
    Selector::new(LOAD_ONE_ENTRY_OF_DISPLAY_ID_TABLE, 2),
    Selector::new(WINDOW_CREATED_OR_DESTROYED_NOTICE_TO_COLOUR_MAP_CHIP, 1),
    Selector::new(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, 7),
    Selector::new(WINDOW_SYSTEM_PIXEL_READ, 2),
    Selector::new(WINDOW_SYSTEM_PIXEL_WRITE, 2),
    Selector::new(WINDOW_SYSTEM_PIXEL_PATH_FINISH, 1),
    Selector::new(WINDOW_SYSTEM_PIXEL_COPY, 4),
    Selector::new(LOAD_WINDOW_SYSTEM_STIPPLE, 1),
    Selector::new(STORE_WINDOW_SYSTEM_STIPPLE, 3),
    Selector::new(ROTATE_WINDOW_SYSTEM_STIPPLE, 3),
    Selector::new(DRAW_ABSOLUTE_POINTS, 1),
    Selector::new(DRAW_ABSOLUTE_POINTS_BURST_OF_6, 6),
    Selector::new(WINDOW_SYSTEM_1_WORD_DATA_PRELOAD, 1),
    Selector::new(WINDOW_SYSTEM_3_WORD_DATA_PRELOAD, 3),
    Selector::new(WINDOW_SYSTEM_5_WORD_DATA_PRELOAD, 5),
    Selector::new(DRAW_ABSOLUTE_POINTS_BURST_OF_24, 24),
    Selector::new(DRAW_POINTS_IN_BACKGROUND_COLOUR, 1),
    Selector::new(DRAW_POINTS_IN_BACKGROUND_COLOUR_BURST_OF_6, 6),
    Selector::new(DRAW_POINTS_IN_BACKGROUND_COLOUR_BURST_OF_24, 24),
    Selector::new(DRAW_RELATIVE_POINTS, 1),
    Selector::new(DRAW_RELATIVE_POINTS_BURST_OF_6, 6),
    Selector::new(DRAW_RELATIVE_POINTS_BURST_OF_24, 24),
    Selector::new(DRAW_LINE_SEGMENTS, 2),
    Selector::new(DRAW_LINE_SEGMENTS_BURST_OF_6, 12),
    // The polyline tokens' word counts are the X server's sender's (established):
    // 1, 7, 2 and 7. A longer polyline arrives as n - 1 words of data preload and
    // the one-word token for the last point (the pipe's consumption is inferred).
    Selector::new(DRAW_CONNECTED_POLYLINE, 1),
    Selector::new(DRAW_CONNECTED_POLYLINE_BURST_OF_6, 7),
    Selector::new(DRAW_CONNECTED_POLYLINE_RELATIVE_POINTS, 2),
    Selector::new(DRAW_CONNECTED_POLYLINE_RELATIVE_POINTS_BURST_OF_6, 7),
    Selector::new(WINDOW_SYSTEM_2_WORD_DATA_PRELOAD, 2),
    Selector::new(WINDOW_SYSTEM_4_WORD_DATA_PRELOAD, 4),
    Selector::new(WINDOW_SYSTEM_6_WORD_DATA_PRELOAD, 6),
    Selector::new(WINDOW_SYSTEM_8_WORD_DATA_PRELOAD, 8),
    Selector::new(WINDOW_SYSTEM_10_WORD_DATA_PRELOAD, 10),
    Selector::new(FILL_BOXES_TWO_CORNERS, 2),
    Selector::new(FILL_BOXES_TWO_CORNERS_BURST_OF_6, 12),
    Selector::new(FILL_RECTANGLES_ORIGIN_WIDTH_HEIGHT, 2),
    Selector::new(FILL_RECTANGLES_ORIGIN_WIDTH_HEIGHT_BURST_OF_6, 12),
    Selector::new(FILL_SPANS, 3),
    Selector::new(FILL_SPANS_BURST_OF_2, 5),
    Selector::new(FILL_SPANS_BURST_OF_6, 13),
    Selector::new(WRITE_GATHER_FLUSH_LAST_SLOT, 1),
    Selector::new(DISPLAY_GENERATOR_REQUEST_CARRIER, 1),
];

/// The GL client's commands, keyed by the low 16 bits of the opening word.
pub(super) const GL: &[Selector] = &[
    Selector::new(CONFIGURE_FRAME_BUFFER_LAYOUT, 10),
    Selector::new(FLUSH_AND_REBUILD_HARDWARE_TOKEN_MAP_FOR_PACKED_VERTEX, 1),
    Selector::new(CONFIGURE_READ_SIDE_FRAME_BUFFER_LAYOUT, 10),
    Selector::new(SELECT_VIDEO_SOURCE, 1),
    Selector::new(SELECT_ACTIVE_TEXTURE_UNIT, 1),
    Selector::new(CHANGE_POST_TEXTURE_SCALE_AND_BIAS, 4),
    Selector::new(LOAD_ONE_ENTRY_OF_ARM_TEXTURE_TABLE, 1),
    Selector::new(RESTART_SELECT_OR_FEEDBACK, 1),
    Selector::new(QUERY_LINE_MODE_LIMIT_TABLE, 1),
    Selector::new(SET_LAZY_VALIDATION_MODE, 1),
    Selector::new(SWITCH_BETWEEN_RENDER_SELECT_AND_FEEDBACK_MODES, 1),
    Selector::new(CLEAR_SELECTION_NAME_STACK, 1),
    Selector::new(REPLACE_TOP_SELECTION_NAME, 1),
    Selector::new(POP_SELECTION_NAME, 1),
    Selector::new(PUSH_SELECTION_NAME, 1),
    Selector::new(SELECT_FEEDBACK_RECORD_TYPE, 1),
    Selector::new(INSERT_FEEDBACK_PASS_THROUGH_MARKER, 1),
    Selector::new(SET_VIEWPORT, 4),
    Selector::new(SET_SCISSOR_BOX, 4),
    Selector::new(RECORD_ERROR_CODE, 1),
    Selector::new(SET_COLOUR_CLEAR_VALUE, 4),
    Selector::new(CLEAR_SELECTED_BUFFERS, 1),
    Selector::new(SET_ACCUMULATION_BUFFER_CLEAR_VALUE, 4),
    Selector::new(ACCUMULATION_BUFFER_OPERATION, 2),
    Selector::new(SET_COLOUR_INDEX_CLEAR_VALUE, 1),
    Selector::new(SET_STENCIL_CLEAR_VALUE, 1),
    Selector::new(SET_DEPTH_CLEAR_VALUE, 2),
    Selector::new(BEGIN_PRIMITIVE_POINTS, 1), // gl_begin_first + 0
    Selector::new(BEGIN_PRIMITIVE_INDEPENDENT_LINES, 1), // gl_begin_first + 1
    Selector::new(BEGIN_PRIMITIVE_LINE_LOOP, 1), // gl_begin_first + 2
    Selector::new(BEGIN_PRIMITIVE_LINE_STRIP, 1), // gl_begin_first + 3
    Selector::new(BEGIN_PRIMITIVE_INDEPENDENT_TRIANGLES, 1), // gl_begin_first + 4
    Selector::new(BEGIN_PRIMITIVE_TRIANGLE_STRIP, 1), // gl_begin_first + 5
    Selector::new(BEGIN_PRIMITIVE_TRIANGLE_FAN, 1), // gl_begin_first + 6
    Selector::new(BEGIN_PRIMITIVE_INDEPENDENT_QUADS, 1), // gl_begin_first + 7
    Selector::new(BEGIN_PRIMITIVE_QUAD_STRIP, 1), // gl_begin_first + 8
    Selector::new(BEGIN_PRIMITIVE_POLYGON, 1),
    Selector::new(END_OPEN_PRIMITIVE, 1),
    Selector::new(BEGIN_PRIMITIVE_RECTANGLE, 1),
    Selector::new(FLUSH_VERTEX_PIPELINE, 1),
    Selector::new(FINISH_DRAIN_PIPE_AND_SIGNAL_COMPLETION, 1),
    Selector::new(FLUSH_RASTER_BOARDS, 1),
    Selector::new(QUERY_PENDING_ERROR, 1),
    Selector::new(QUERY_CURRENT_VERTEX_ATTRIBUTES, 1),
    Selector::new(QUERY_RASTER_POSITION_STATE, 1),
    Selector::new(LOAD_ENGINE_TRANSFORM_ATTRIBUTES, 1),
    Selector::new(POP_TEXTURE_GENERATION_STATE, 1),
    Selector::new(SET_DEPTH_RANGE, 4),
    Selector::new(SELECT_CURRENT_MATRIX_STACK, 1),
    Selector::new(MULTIPLY_CURRENT_MATRIX_BY_FRUSTUM, 12),
    Selector::new(LOAD_IDENTITY_MATRIX, 1),
    Selector::new(LOAD_MATRIX_16_FLOATS, 16),
    Selector::new(LOAD_MATRIX_16_DOUBLES, 32),
    Selector::new(MULTIPLY_BY_MATRIX_16_FLOATS, 16),
    Selector::new(MULTIPLY_BY_MATRIX_16_DOUBLES, 32),
    Selector::new(MULTIPLY_CURRENT_MATRIX_BY_ORTHOGRAPHIC_PROJECTION, 12),
    Selector::new(POP_MATRIX_STACK, 1),
    Selector::new(PUSH_MATRIX_STACK, 1),
    Selector::new(ROTATE_DOUBLES, 8),
    Selector::new(ROTATE_FLOATS, 4),
    Selector::new(SCALE_DOUBLES, 6),
    Selector::new(SCALE_FLOATS, 3),
    Selector::new(TRANSLATE_DOUBLES, 6),
    Selector::new(TRANSLATE_FLOATS, 3),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_SIGNED_BYTE, 1),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_DOUBLE, 6),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_SINGLE_FLOAT, 3),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_SIGNED_32_BIT_INTEGER, 3),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_UNSIGNED_BYTE, 1),
    Selector::new(SET_CURRENT_COLOUR_3_COMPONENTS_UNSIGNED_32_BIT_INTEGER, 3),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_DOUBLE, 8),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_SINGLE_FLOAT, 4),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_SIGNED_32_BIT_INTEGER, 4),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_SIGNED_SHORT, 2),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_UNSIGNED_BYTE, 1),
    Selector::new(SET_CURRENT_COLOUR_4_COMPONENTS_UNSIGNED_32_BIT_INTEGER, 4),
    Selector::new(SET_CURRENT_COLOUR_INDEX_DOUBLE, 2),
    Selector::new(SET_CURRENT_COLOUR_INDEX_SINGLE_FLOAT, 1),
    Selector::new(SET_CURRENT_COLOUR_INDEX_SIGNED_32_BIT_INTEGER, 1),
    Selector::new(SET_CURRENT_COLOUR_INDEX_SIGNED_SHORT, 1),
    Selector::new(SET_CURRENT_NORMAL_3_COMPONENTS_SIGNED_BYTE, 1),
    Selector::new(SET_CURRENT_NORMAL_3_COMPONENTS_DOUBLE, 6),
    Selector::new(SET_CURRENT_NORMAL_3_COMPONENTS_SINGLE_FLOAT, 3),
    Selector::new(SET_CURRENT_NORMAL_3_COMPONENTS_SIGNED_32_BIT_INTEGER, 3),
    Selector::new(SET_CURRENT_NORMAL_3_COMPONENTS_SIGNED_SHORT, 1),
    Selector::new(SET_CURRENT_TEXTURE_COORDINATE_1_COMPONENTS_SINGLE_FLOAT, 1),
    Selector::new(SET_CURRENT_TEXTURE_COORDINATE_2_COMPONENTS_SINGLE_FLOAT, 2),
    Selector::new(SET_CURRENT_TEXTURE_COORDINATE_2_COMPONENTS_SIGNED_SHORT, 1),
    Selector::new(SET_CURRENT_TEXTURE_COORDINATE_3_COMPONENTS_SINGLE_FLOAT, 3),
    Selector::new(SET_CURRENT_TEXTURE_COORDINATE_4_COMPONENTS_SINGLE_FLOAT, 4),
    Selector::new(SUBMIT_VERTEX_2_COMPONENTS_DOUBLE, 4),
    Selector::new(SUBMIT_VERTEX_2_COMPONENTS_SINGLE_FLOAT, 2),
    Selector::new(SUBMIT_VERTEX_2_COMPONENTS_SIGNED_32_BIT_INTEGER, 2),
    Selector::new(SUBMIT_VERTEX_2_COMPONENTS_SIGNED_SHORT, 1),
    Selector::new(SUBMIT_VERTEX_3_COMPONENTS_DOUBLE, 6),
    Selector::new(SUBMIT_VERTEX_3_COMPONENTS_SINGLE_FLOAT, 3),
    Selector::new(SUBMIT_VERTEX_3_COMPONENTS_SIGNED_32_BIT_INTEGER, 3),
    Selector::new(SUBMIT_VERTEX_3_COMPONENTS_SIGNED_SHORT, 1),
    Selector::new(SUBMIT_VERTEX_4_COMPONENTS_DOUBLE, 8),
    Selector::new(SUBMIT_VERTEX_4_COMPONENTS_SINGLE_FLOAT, 4),
    Selector::new(SUBMIT_VERTEX_4_COMPONENTS_SIGNED_32_BIT_INTEGER, 4),
    Selector::new(SUBMIT_VERTEX_4_COMPONENTS_SIGNED_SHORT, 2),
    Selector::new(SET_CURRENT_EDGE_FLAG, 1),
    Selector::new(SET_RASTER_POSITION_2_COMPONENTS_FLOAT, 2),
    Selector::new(SET_RASTER_POSITION_3_COMPONENTS_FLOAT, 3),
    Selector::new(SET_RASTER_POSITION_4_COMPONENTS_FLOAT, 4),
    Selector::new(SET_CURRENT_RASTER_POSITION, 15),
    Selector::new(SET_UP_1D_EVALUATOR_GRID, 4),
    Selector::new(SET_UP_2D_EVALUATOR_GRID, 8),
    Selector::new(SKIP_2_WORDS, 1),
    Selector::new(SKIP_4_WORDS, 3),
    Selector::new(SKIP_5_WORDS, 4),
    Selector::new(SET_POINT_PARAMETER, 2),
    Selector::new(SET_POINT_PARAMETERS, 4),
    Selector::new(SET_SPRITE_PARAMETER, 2),
    Selector::new(SET_SPRITE_PARAMETERS, 4),
    Selector::new(SET_MULTISAMPLE_COVERAGE_MASK, 2),
    Selector::new(SET_MULTISAMPLE_PATTERN, 1),
    Selector::new(LOAD_1D_CONVOLUTION_FILTER, 7),
    Selector::new(LOAD_2D_CONVOLUTION_FILTER, 8),
    Selector::new(COPY_1D_CONVOLUTION_FILTER_FROM_FRAME_BUFFER, 4),
    Selector::new(COPY_2D_CONVOLUTION_FILTER_FROM_FRAME_BUFFER, 5),
    Selector::new(QUERY_CONVOLUTION_FILTER, 3),
    Selector::new(LOAD_SEPARABLE_2D_FILTER, 9),
    Selector::new(QUERY_SEPARABLE_FILTER, 3),
    Selector::new(SET_PIXEL_TRANSFORM_PARAMETERS_INTEGER, 3),
    Selector::new(SET_PIXEL_TRANSFORM_PARAMETERS_FLOAT, 6),
    Selector::new(RESOLVE_WHETHER_PIXEL_FORMAT_IS_SINGLE_COMPONENT, 4),
    Selector::new(LOAD_COLOUR_TABLE, 9),
    Selector::new(QUERY_COLOUR_TABLE, 4),
    Selector::new(SET_COLOUR_TABLE_PARAMETERS, 6),
    Selector::new(QUERY_COLOUR_TABLE_PARAMETERS, 2),
    Selector::new(SET_UP_HISTOGRAM, 5),
    Selector::new(RESET_HISTOGRAM, 1),
    Selector::new(QUERY_HISTOGRAM, 1),
    Selector::new(QUERY_HISTOGRAM_PARAMETERS, 2),
    Selector::new(SET_UP_MIN_MAX_TRACKING, 2),
    Selector::new(RESET_MIN_MAX_TRACKING, 1),
    Selector::new(QUERY_MIN_MAX_RESULTS, 1),
    Selector::new(QUERY_MIN_MAX_PARAMETERS, 1),
    Selector::new(SET_LIGHT_PARAMETER_FLOAT_FORM, 6),
    Selector::new(SET_LIGHT_PARAMETER_INTEGER_FORM, 6),
    Selector::new(SET_LIGHTING_MODEL_PARAMETER_FLOAT_FORM, 5),
    Selector::new(SET_LIGHTING_MODEL_PARAMETER_INTEGER_FORM, 5),
    Selector::new(SET_MATERIAL_PARAMETER_FLOAT_FORM, 6),
    Selector::new(SET_COLOUR_MATERIAL_TRACKING, 2),
    Selector::new(ENABLE_CAPABILITY, 1),
    Selector::new(DISABLE_CAPABILITY, 1),
    Selector::new(SET_FLAT_OR_SMOOTH_SHADING, 1),
    Selector::new(SET_DEPTH_TEST, 1),
    Selector::new(SET_DEPTH_WRITE_MASK, 1),
    Selector::new(SET_BLEND_FACTORS, 2),
    Selector::new(SET_CONSTANT_BLEND_COLOUR, 4),
    Selector::new(SET_BLEND_EQUATION, 1),
    Selector::new(SET_ALPHA_TEST, 2),
    Selector::new(SET_STENCIL_TEST, 3),
    Selector::new(SET_STENCIL_OPERATIONS, 3),
    Selector::new(SET_STENCIL_WRITE_MASK, 1),
    Selector::new(SET_COLOUR_WRITE_MASK, 4),
    Selector::new(SET_COLOUR_INDEX_WRITE_MASK, 1),
    Selector::new(SET_LOGIC_OPERATION, 1),
    Selector::new(SET_FACE_CULLING, 1),
    Selector::new(SET_FRONT_FACE_WINDING, 1),
    Selector::new(SET_POLYGON_FILL_MODE, 2),
    Selector::new(SET_POLYGON_OFFSET, 2),
    Selector::new(SET_POLYGON_OFFSET_EXTENSION_FORM, 2),
    Selector::new(LOAD_POLYGON_STIPPLE_PATTERN, 3),
    Selector::new(SET_LINE_WIDTH, 2),
    Selector::new(SET_POINT_SIZE, 2),
    Selector::new(SET_LINE_STIPPLE, 2),
    Selector::new(SET_FOG_PARAMETER_FLOAT_FORM, 5),
    Selector::new(SET_FOG_PARAMETER_INTEGER_FORM, 5),
    Selector::new(SET_USER_CLIP_PLANE, 9),
    Selector::new(SET_QUALITY_HINT, 2),
    Selector::new(SELECT_DRAW_BUFFER, 1),
    Selector::new(SELECT_READ_BUFFER, 1),
    Selector::new(SET_PIXEL_ZOOM_FACTORS, 2),
    Selector::new(SET_PIXEL_TRANSFER_PARAMETER_FLOAT_FORM, 2),
    Selector::new(SET_PIXEL_STORE_PARAMETER_INTEGER_FORM, 2),
    Selector::new(LOAD_PIXEL_MAP_FLOAT_VALUES, 4),
    Selector::new(LOAD_PIXEL_MAP_UNSIGNED_32_BIT_VALUES, 4),
    Selector::new(LOAD_PIXEL_MAP_UNSIGNED_16_BIT_VALUES, 4),
    Selector::new(SET_TEXTURE_ENVIRONMENT_PARAMETER_FLOAT_FORM, 5),
    Selector::new(SET_TEXTURE_ENVIRONMENT_PARAMETER_INTEGER_FORM, 5),
    Selector::new(DRAW_BITMAP_FROM_DISPLAY_LIST, 8),
    Selector::new(DRAW_BITMAP, 9),
    Selector::new(DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, 8),
    Selector::new(READ_PIXEL_RECTANGLE_BACK_TO_HOST, 7),
    Selector::new(COPY_PIXEL_RECTANGLE_WITHIN_FRAME_BUFFER, 7),
    Selector::new(ASK_WHETHER_COPY_SOURCE_OVERLAPS_ITS_DESTINATION, 4),
    Selector::new(QUERY_TEXTURE_IMAGE, 18),
    Selector::new(SUSPEND_PIXEL_MODE_STATE_AROUND_TRANSFER, 2),
    Selector::new(ASK_WHETHER_CONVOLUTION_COPY_OVERLAPS, 4),
    Selector::new(QUERY_STATE_GENERIC, 1),
    // The typed queries. The float forms are the ones the client sends (the guest's token stream
    // shows each: a light, material or texture-generation query is two words, the name after the
    // light number, face or coordinate; a texture-environment, clip-plane or polygon-stipple query
    // one). The firmware has no handler for the integer forms, the generic float and integer
    // forms and the double forms, which is why the client converts the float reply itself; they are
    // framed with their float sibling's count so that a stray send cannot misalign the words after
    // it (inferred: no sender is known). The pixel-map count is the one `answer` reads (inferred).
    Selector::new(QUERY_CLIP_PLANE, 1),
    Selector::new(QUERY_STATE_FLOAT, 1),
    Selector::new(QUERY_STATE_INTEGER, 1),
    Selector::new(QUERY_LIGHT_FLOAT, 2),
    Selector::new(QUERY_LIGHT_INTEGER, 2),
    Selector::new(QUERY_MATERIAL_FLOAT, 2),
    Selector::new(QUERY_MATERIAL_INTEGER, 2),
    Selector::new(QUERY_PIXEL_MAP_FLOAT, 3),
    Selector::new(QUERY_POLYGON_STIPPLE, 1),
    Selector::new(QUERY_TEXTURE_ENVIRONMENT_FLOAT, 1),
    Selector::new(QUERY_TEXTURE_ENVIRONMENT_INTEGER, 1),
    Selector::new(QUERY_TEXTURE_GENERATION_FLOAT, 2),
    Selector::new(PUSH_ATTRIBUTE_STACK, 1),
    Selector::new(POP_ATTRIBUTE_STACK, 1),
    Selector::new(POP_SAVED_LIGHT_STATE, 1),
    Selector::new(BIND_ARM_TEXTURE_CACHE_ENTRY, 1),
    Selector::new(FREE_ARM_TEXTURE_CACHE_ENTRY, 1),
    Selector::new(DRAW_TEXELS_INTO_TEXTURE_MEMORY, 20),
    Selector::new(SET_TEXEL_STORE_PARAMETER, 2),
    Selector::new(TEXTURE_LOAD_BY_IDENTIFIER, 1),
    Selector::new(BIND_TEXTURE_BY_IDENTIFIER, 1),
    Selector::new(BIND_2D_MIP_MAPPED_TEXTURE, 22),
    Selector::new(BIND_3D_MIP_MAPPED_TEXTURE, 14),
    Selector::new(BIND_3D_TEXTURE, 4),
    Selector::new(BIND_DETAIL_TEXTURE, 6),
    Selector::new(BIND_BICUBIC_TEXTURE, 5),
    Selector::new(BIND_2D_CLIP_MAP_TEXTURE, 8),
    Selector::new(SET_SHADOW_TEXTURE_STATE, 3),
    Selector::new(TEXTURE_LOAD_SET_UP, 11),
    Selector::new(BIND_TEXTURE_CURRENT_FORM, 5),
    // Five words, as the table's count and text agree; the recorded GL demos frame correctly with five and misframe with seven.
    Selector::new(BIND_LINEAR_TEXTURE, 5),
    Selector::new(LOAD_POST_TEXTURE_SCALE_AND_BIAS, 4),
    Selector::new(INVALIDATE_ARM_TEXTURE_CACHE_INDEX, 2),
    Selector::new(SWITCH_LEGACY_API_COMPATIBILITY_MODE, 1),
    Selector::new(SET_COLOUR_MATERIAL_VALUE, 1),
    Selector::new(SWAP_TRIANGLE_MESH_VERTEX, 1),
    Selector::new(SET_COLOUR_NORMAL_MODE_VALUE, 1),
    Selector::new(SET_TRIANGLE_MESH_MODE, 1),
    Selector::new(TEXTURE_COORDINATE_GENERATION_FLOAT_FORM, 9),
    Selector::new(SET_RASTER_POSITION_2_COMPONENTS_DOUBLE, 4),
    Selector::new(SET_RASTER_POSITION_2_COMPONENTS_32_BIT_INTEGER, 2),
    Selector::new(SET_RASTER_POSITION_2_COMPONENTS_SHORT, 2),
    Selector::new(SET_RASTER_POSITION_3_COMPONENTS_DOUBLE, 6),
    Selector::new(SET_RASTER_POSITION_3_COMPONENTS_32_BIT_INTEGER, 3),
    Selector::new(SET_RASTER_POSITION_3_COMPONENTS_SHORT, 3),
    Selector::new(SET_RASTER_POSITION_4_COMPONENTS_DOUBLE, 8),
    Selector::new(SET_RASTER_POSITION_4_COMPONENTS_32_BIT_INTEGER, 4),
    Selector::new(SET_RASTER_POSITION_4_COMPONENTS_SHORT, 4),
    Selector::new(SET_VERTEX_ARRAY_COMMAND_TABLE, 1),
    Selector::new(VERTEX_ARRAY_SET_UP, 1),
    Selector::new(BEGIN_VERTEX_ARRAY_LOOP, 3),
    Selector::new(VERTEX_ARRAY_TEAR_DOWN, 1),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_1, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_2, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_3, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_4, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_5, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_6, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_7, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_8, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_9, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_10, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_11, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_12, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_13, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_14, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_15, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_16, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_17, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_18, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_19, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_20, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_21, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_22, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_23, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_24, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_25, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_26, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_27, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_28, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_29, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_30, 0),
    Selector::new(VERTEX_ARRAY_LOOP_ENTRY_31, 0),
];

/// The video command table: the data words each command carries between its
/// number and its checksum (established for all 93 requests). Each entry's comment is the
/// request's name in the kona repo's generated `dispreq` unit.
pub(super) const VIDEO: &[Option<u16>] = &[
    Some(1), // 0x00 DISPREQ_LOOP_BACK_TEST
    Some(2), // 0x01 DISPREQ_WRITE_A_DISPLAY_BUS_WORD
    Some(1), // 0x02 DISPREQ_READ_A_DISPLAY_BUS_WORD
    Some(10), // 0x03 DISPREQ_DIAGNOSTIC_SUBSTITUTE_COLOUR_SET
    Some(1), // 0x04 DISPREQ_DIAGNOSTIC_SUBSTITUTE_COLOUR_OFF
    Some(4), // 0x05 DISPREQ_BUS_TRACE
    Some(4107), // 0x06 DISPREQ_DIAGNOSTIC_CURSOR_FROM_IMAGE
    Some(1), // 0x07 DISPREQ_FIRMWARE_DEBUG_PRINT_MASK
    Some(3), // 0x08 DISPREQ_FRAME_BUFFER_TILE_SET_UP
    Some(1), // 0x09 DISPREQ_INPUT_SYNC_SOURCE_SELECT
    Some(1), // 0x0a DISPREQ_INPUT_SYNC_SOURCE_READ
    Some(2), // 0x0b DISPREQ_HORIZONTAL_PHASE_SET
    Some(1), // 0x0c DISPREQ_HORIZONTAL_PHASE_READ
    Some(1), // 0x0d DISPREQ_VERTICAL_PHASE_SET
    Some(1), // 0x0e DISPREQ_VERTICAL_PHASE_READ
    Some(2), // 0x0f DISPREQ_CHANNEL_GAMMA_MAP_SELECT
    Some(1), // 0x10 DISPREQ_CHANNEL_GAMMA_MAP_READ
    Some(1), // 0x11 DISPREQ_REBUILD_DEFAULT_DISPLAY_STATE
    Some(1), // 0x12 DISPREQ_BOARD_CONFIGURATION_READ
    Some(1), // 0x13 DISPREQ_CHANNEL_CONFIGURATION_READ
    Some(8), // 0x14 DISPREQ_CHANNEL_RESIZE
    Some(1), // 0x15 DISPREQ_CHANNEL_RESIZE_READ
    Some(383), // 0x16 DISPREQ_LOAD_A_VIDEO_FORMAT
    Some(3), // 0x17 DISPREQ_ENABLE_A_CHANNEL
    Some(1), // 0x18 DISPREQ_DISABLE_A_CHANNEL
    Some(16), // 0x19 DISPREQ_LOAD_GENLOCK_PARAMETERS
    Some(1), // 0x1a DISPREQ_ENABLE_GENLOCK
    Some(2), // 0x1b DISPREQ_CURSOR_POSITION
    Some(1), // 0x1c DISPREQ_CURSOR_CHANNEL_SELECT
    Some(1), // 0x1d DISPREQ_CURSOR_TYPE
    Some(2), // 0x1e DISPREQ_CURSOR_PRIORITY
    Some(2), // 0x1f DISPREQ_CHANNEL_BLANKING_SET
    Some(1), // 0x20 DISPREQ_CHANNEL_BLANKING_READ
    Some(2), // 0x21 DISPREQ_OUTPUT_GAIN_SET
    Some(1), // 0x22 DISPREQ_OUTPUT_GAIN_READ
    Some(4), // 0x23 DISPREQ_SYNC_OUTPUT_SELECT
    Some(1), // 0x24 DISPREQ_SYNC_OUTPUT_READ
    Some(2), // 0x25 DISPREQ_OUTPUT_PEDESTAL_SET
    Some(1), // 0x26 DISPREQ_OUTPUT_PEDESTAL_READ
    Some(1), // 0x27 DISPREQ_TEXT_PORT_ID_ENTRY_INITIALISE
    Some(1), // 0x28 DISPREQ_TEXT_PORT_GLYPH_LOAD
    Some(4), // 0x29 DISPREQ_TEXT_PORT_COLOUR_INDEX_LOAD
    Some(1), // 0x2a DISPREQ_VIDEO_OUTPUT_CHIP_READ_BACK_ENABLE
    Some(1), // 0x2b DISPREQ_MASTER_CLOCK_SELECT
    Some(1), // 0x2c DISPREQ_MASTER_CLOCK_SELECT_READ
    Some(1), // 0x2d DISPREQ_AUTO_INITIALISE_FROM_STORED_COMBINATION
    Some(1), // 0x2e DISPREQ_ENCODER_LOAD
    Some(1), // 0x2f DISPREQ_ENCODER_OUTPUT_GAIN
    Some(1), // 0x30 DISPREQ_ENCODER_OUTPUT_GAIN_READ
    Some(1), // 0x31 DISPREQ_SUB_CARRIER_PHASE_SET
    Some(1), // 0x32 DISPREQ_SUB_CARRIER_PHASE_READ
    Some(1), // 0x33 DISPREQ_ENCODER_TEST_PATTERN
    Some(2), // 0x34 DISPREQ_BIND_A_CHANNEL_TO_A_WINDOW_ID
    Some(1328), // 0x35 DISPREQ_LOAD_BOARD_LOGIC_SERIALLY
    Some(4), // 0x36 DISPREQ_OPTION_BOARD_SIZE_REGISTERS
    Some(2), // 0x37 DISPREQ_OPTION_BOARD_CONTROL_REGISTERS
    Some(263), // 0x38 DISPREQ_DAC_CURSOR_LOAD
    Some(7), // 0x39 DISPREQ_FIELD_DOMINANCE_AND_COPY_FLAGS
    Some(1), // 0x3a DISPREQ_COPY_CHANNEL_INFORMATION_READ
    Some(1), // 0x3b DISPREQ_FAST_LIST_CHUNK_SIZE
    Some(1), // 0x3c DISPREQ_TEMPORARY_BLANK_ALL_CHANNELS
    Some(1), // 0x3d DISPREQ_DISPLAY_INTERRUPT_STATE_READ
    Some(1), // 0x3e DISPREQ_XMAP_CLOCK_SELECT
    Some(1), // 0x3f DISPREQ_XMAP_CLOCK_SELECT_READ
    Some(3), // 0x40 DISPREQ_CHANNEL_RESET_RATIOS
    Some(1), // 0x41 DISPREQ_RESET_THE_RASTER_BOARDS
    Some(1), // 0x42 DISPREQ_CHANNEL_ENABLES_READ
    Some(9), // 0x43 DISPREQ_PLACEMENT_RESOLUTION
    Some(1), // 0x44 DISPREQ_GENLOCK_SWITCHING_SET
    Some(1), // 0x45 DISPREQ_GENLOCK_SWITCHING_READ
    Some(2), // 0x46 DISPREQ_CHANNEL_FIELD_DOMINANCE_SET
    Some(1), // 0x47 DISPREQ_CHANNEL_FIELD_DOMINANCE_READ
    Some(1), // 0x48 DISPREQ_BOARD_RESET_RATIO_SET
    Some(1), // 0x49 DISPREQ_BOARD_RESET_RATIO_READ
    Some(1), // 0x4a DISPREQ_CHANNEL_FORMAT_INFORMATION_READ
    Some(5), // 0x4b DISPREQ_CHANNEL_GAMMA_MAP_STATE_SET
    Some(1), // 0x4c DISPREQ_CHANNEL_GAMMA_MAP_STATE_READ
    Some(1), // 0x4d DISPREQ_ENCODER_ENABLE
    Some(2), // 0x4e DISPREQ_RESIZE_SYNC_TYPE
    Some(7), // 0x4f DISPREQ_COPY_ANOTHER_CHANNEL_OUTPUT
    Some(1), // 0x50 DISPREQ_MONITOR_IDENTITY_QUERY
    Some(34), // 0x51 DISPREQ_MONITOR_COMMAND
    Some(6), // 0x52 DISPREQ_OPTION_BOARD_LOGIC_IMAGE_LOAD
    Some(1), // 0x53 DISPREQ_WINDOW_SYSTEM_VIDEO_INITIALISE
    Some(1), // 0x54 DISPREQ_INTERNAL_GENLOCK_SET
    Some(1), // 0x55 DISPREQ_INTERNAL_GENLOCK_READ
    Some(3), // 0x56 DISPREQ_BUFFER_PARAMETERS
    Some(1), // 0x57 DISPREQ_BUFFER_PARAMETERS_READ
    Some(3), // 0x58 DISPREQ_DATA_FORMAT_SET
    Some(1), // 0x59 DISPREQ_DATA_FORMAT_READ
    Some(1), // 0x5a DISPREQ_DOUBLE_FREQUENCY_PIXEL_OPTION
    Some(42), // 0x5b DISPREQ_OPTION_BOARD_TWO_WIRE_PACKET
    Some(1), // 0x5c DISPREQ_OPTION_BOARD_REPLY_READ
];

/// The reply each video command waits for, by command number: the reply id and the bytes of data
/// after it, or `None` for a command nobody waits on. Established for all 93 requests (32 of them
/// reply; the monitor command, which carries a payload, does not).
pub(super) const VIDEO_REPLY: &[Option<(u8, u8)>] = &[
    Some((0, 4)), // 0x00 DISPREQ_LOOP_BACK_TEST
    None, // 0x01 DISPREQ_WRITE_A_DISPLAY_BUS_WORD
    Some((1, 4)), // 0x02 DISPREQ_READ_A_DISPLAY_BUS_WORD
    None, // 0x03 DISPREQ_DIAGNOSTIC_SUBSTITUTE_COLOUR_SET
    None, // 0x04 DISPREQ_DIAGNOSTIC_SUBSTITUTE_COLOUR_OFF
    None, // 0x05 DISPREQ_BUS_TRACE
    None, // 0x06 DISPREQ_DIAGNOSTIC_CURSOR_FROM_IMAGE
    None, // 0x07 DISPREQ_FIRMWARE_DEBUG_PRINT_MASK
    None, // 0x08 DISPREQ_FRAME_BUFFER_TILE_SET_UP
    None, // 0x09 DISPREQ_INPUT_SYNC_SOURCE_SELECT
    Some((2, 4)), // 0x0a DISPREQ_INPUT_SYNC_SOURCE_READ
    None, // 0x0b DISPREQ_HORIZONTAL_PHASE_SET
    Some((3, 4)), // 0x0c DISPREQ_HORIZONTAL_PHASE_READ
    None, // 0x0d DISPREQ_VERTICAL_PHASE_SET
    Some((4, 4)), // 0x0e DISPREQ_VERTICAL_PHASE_READ
    None, // 0x0f DISPREQ_CHANNEL_GAMMA_MAP_SELECT
    Some((5, 4)), // 0x10 DISPREQ_CHANNEL_GAMMA_MAP_READ
    None, // 0x11 DISPREQ_REBUILD_DEFAULT_DISPLAY_STATE
    Some((6, 20)), // 0x12 DISPREQ_BOARD_CONFIGURATION_READ
    Some((7, 76)), // 0x13 DISPREQ_CHANNEL_CONFIGURATION_READ
    None, // 0x14 DISPREQ_CHANNEL_RESIZE
    Some((8, 72)), // 0x15 DISPREQ_CHANNEL_RESIZE_READ
    None, // 0x16 DISPREQ_LOAD_A_VIDEO_FORMAT
    None, // 0x17 DISPREQ_ENABLE_A_CHANNEL
    None, // 0x18 DISPREQ_DISABLE_A_CHANNEL
    None, // 0x19 DISPREQ_LOAD_GENLOCK_PARAMETERS
    None, // 0x1a DISPREQ_ENABLE_GENLOCK
    None, // 0x1b DISPREQ_CURSOR_POSITION
    None, // 0x1c DISPREQ_CURSOR_CHANNEL_SELECT
    None, // 0x1d DISPREQ_CURSOR_TYPE
    None, // 0x1e DISPREQ_CURSOR_PRIORITY
    None, // 0x1f DISPREQ_CHANNEL_BLANKING_SET
    Some((9, 4)), // 0x20 DISPREQ_CHANNEL_BLANKING_READ
    None, // 0x21 DISPREQ_OUTPUT_GAIN_SET
    Some((10, 4)), // 0x22 DISPREQ_OUTPUT_GAIN_READ
    None, // 0x23 DISPREQ_SYNC_OUTPUT_SELECT
    Some((11, 24)), // 0x24 DISPREQ_SYNC_OUTPUT_READ
    None, // 0x25 DISPREQ_OUTPUT_PEDESTAL_SET
    Some((12, 4)), // 0x26 DISPREQ_OUTPUT_PEDESTAL_READ
    None, // 0x27 DISPREQ_TEXT_PORT_ID_ENTRY_INITIALISE
    None, // 0x28 DISPREQ_TEXT_PORT_GLYPH_LOAD
    None, // 0x29 DISPREQ_TEXT_PORT_COLOUR_INDEX_LOAD
    None, // 0x2a DISPREQ_VIDEO_OUTPUT_CHIP_READ_BACK_ENABLE
    None, // 0x2b DISPREQ_MASTER_CLOCK_SELECT
    Some((13, 4)), // 0x2c DISPREQ_MASTER_CLOCK_SELECT_READ
    None, // 0x2d DISPREQ_AUTO_INITIALISE_FROM_STORED_COMBINATION
    None, // 0x2e DISPREQ_ENCODER_LOAD
    None, // 0x2f DISPREQ_ENCODER_OUTPUT_GAIN
    Some((14, 4)), // 0x30 DISPREQ_ENCODER_OUTPUT_GAIN_READ
    None, // 0x31 DISPREQ_SUB_CARRIER_PHASE_SET
    Some((15, 4)), // 0x32 DISPREQ_SUB_CARRIER_PHASE_READ
    None, // 0x33 DISPREQ_ENCODER_TEST_PATTERN
    None, // 0x34 DISPREQ_BIND_A_CHANNEL_TO_A_WINDOW_ID
    None, // 0x35 DISPREQ_LOAD_BOARD_LOGIC_SERIALLY
    None, // 0x36 DISPREQ_OPTION_BOARD_SIZE_REGISTERS
    None, // 0x37 DISPREQ_OPTION_BOARD_CONTROL_REGISTERS
    None, // 0x38 DISPREQ_DAC_CURSOR_LOAD
    None, // 0x39 DISPREQ_FIELD_DOMINANCE_AND_COPY_FLAGS
    Some((16, 24)), // 0x3a DISPREQ_COPY_CHANNEL_INFORMATION_READ
    None, // 0x3b DISPREQ_FAST_LIST_CHUNK_SIZE
    None, // 0x3c DISPREQ_TEMPORARY_BLANK_ALL_CHANNELS
    Some((17, 12)), // 0x3d DISPREQ_DISPLAY_INTERRUPT_STATE_READ
    None, // 0x3e DISPREQ_XMAP_CLOCK_SELECT
    Some((18, 4)), // 0x3f DISPREQ_XMAP_CLOCK_SELECT_READ
    None, // 0x40 DISPREQ_CHANNEL_RESET_RATIOS
    None, // 0x41 DISPREQ_RESET_THE_RASTER_BOARDS
    Some((19, 8)), // 0x42 DISPREQ_CHANNEL_ENABLES_READ
    None, // 0x43 DISPREQ_PLACEMENT_RESOLUTION
    None, // 0x44 DISPREQ_GENLOCK_SWITCHING_SET
    Some((20, 4)), // 0x45 DISPREQ_GENLOCK_SWITCHING_READ
    None, // 0x46 DISPREQ_CHANNEL_FIELD_DOMINANCE_SET
    Some((21, 4)), // 0x47 DISPREQ_CHANNEL_FIELD_DOMINANCE_READ
    None, // 0x48 DISPREQ_BOARD_RESET_RATIO_SET
    Some((22, 4)), // 0x49 DISPREQ_BOARD_RESET_RATIO_READ
    Some((23, 20)), // 0x4a DISPREQ_CHANNEL_FORMAT_INFORMATION_READ
    None, // 0x4b DISPREQ_CHANNEL_GAMMA_MAP_STATE_SET
    Some((24, 16)), // 0x4c DISPREQ_CHANNEL_GAMMA_MAP_STATE_READ
    None, // 0x4d DISPREQ_ENCODER_ENABLE
    None, // 0x4e DISPREQ_RESIZE_SYNC_TYPE
    None, // 0x4f DISPREQ_COPY_ANOTHER_CHANNEL_OUTPUT
    Some((25, 68)), // 0x50 DISPREQ_MONITOR_IDENTITY_QUERY
    None, // 0x51 DISPREQ_MONITOR_COMMAND
    Some((27, 4)), // 0x52 DISPREQ_OPTION_BOARD_LOGIC_IMAGE_LOAD
    None, // 0x53 DISPREQ_WINDOW_SYSTEM_VIDEO_INITIALISE
    None, // 0x54 DISPREQ_INTERNAL_GENLOCK_SET
    Some((28, 4)), // 0x55 DISPREQ_INTERNAL_GENLOCK_READ
    None, // 0x56 DISPREQ_BUFFER_PARAMETERS
    Some((29, 8)), // 0x57 DISPREQ_BUFFER_PARAMETERS_READ
    None, // 0x58 DISPREQ_DATA_FORMAT_SET
    Some((30, 8)), // 0x59 DISPREQ_DATA_FORMAT_READ
    None, // 0x5a DISPREQ_DOUBLE_FREQUENCY_PIXEL_OPTION
    None, // 0x5b DISPREQ_OPTION_BOARD_TWO_WIRE_PACKET
    Some((31, 164)), // 0x5c DISPREQ_OPTION_BOARD_REPLY_READ
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every selector of the kernel and GL tables is a host token the definitions
    /// name, once, and the table's own entries do not repeat a token.
    #[test]
    fn the_selector_tables_and_the_definitions_agree() {
        for (what, table) in [("kernel", KERNEL), ("GL", GL), ("dma fifo", DMA_FIFO)] {
            let mut seen = std::collections::BTreeSet::new();
            for s in table {
                assert!(!token_name(s.selector).is_empty(), "{what} table: {:#x} is not a token", s.selector);
                assert!(seen.insert(s.selector), "{what} table: {:#x} ({}) twice", s.selector, token_name(s.selector));
            }
        }
        // The textport grammar is its own: its numbers are not the tokens'.
        assert!(TEXTPORT.iter().all(|s| s.selector < 0x10));
    }

    /// The four polyline tokens carry 1, 7, 2 and 7 words (the X server's
    /// sender counts). A one-word token that framed as two would pull the next
    /// command's token word in as a point and desync everything after it.
    #[test]
    fn the_polyline_tokens_frame_one_seven_two_and_seven_words() {
        let argc = |sel| KERNEL.iter().find(|s| s.selector == sel).map(|s| s.argc);
        assert_eq!(argc(DRAW_CONNECTED_POLYLINE), Some(1));
        assert_eq!(argc(DRAW_CONNECTED_POLYLINE_BURST_OF_6), Some(7));
        assert_eq!(argc(DRAW_CONNECTED_POLYLINE_RELATIVE_POINTS), Some(2));
        assert_eq!(argc(DRAW_CONNECTED_POLYLINE_RELATIVE_POINTS_BURST_OF_6), Some(7));
    }
}
