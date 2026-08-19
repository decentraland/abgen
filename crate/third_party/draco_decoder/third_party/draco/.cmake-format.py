with section('parse'):
  additional_commands = {
      'draco_add_emscripten_executable': {
          'kwargs': {
              'NAME': '*',
              'SOURCES': '*',
              'OUTPUT_NAME': '*',
              'DEFINES': '*',
              'INCLUDES': '*',
              'COMPILE_FLAGS': '*',
              'LINK_FLAGS': '*',
              'OBJLIB_DEPS': '*',
              'LIB_DEPS': '*',
              'GLUE_PATH': '*',
              'PRE_LINK_JS_SOURCES': '*',
              'POST_LINK_JS_SOURCES': '*',
              'FEATURES': '*',
          },
          'pargs': 0,
      },
      'draco_add_executable': {
          'kwargs': {
              'NAME': '*',
              'SOURCES': '*',
              'OUTPUT_NAME': '*',
              'TEST': 0,
              'DEFINES': '*',
              'INCLUDES': '*',
              'COMPILE_FLAGS': '*',
              'LINK_FLAGS': '*',
              'OBJLIB_DEPS': '*',
              'LIB_DEPS': '*',
          },
          'pargs': 0,
      },
      'draco_add_library': {
          'kwargs': {
              'NAME': '*',
              'TYPE': '*',
              'SOURCES': '*',
              'TEST': 0,
              'OUTPUT_NAME': '*',
              'DEFINES': '*',
              'INCLUDES': '*',
              'COMPILE_FLAGS': '*',
              'LINK_FLAGS': '*',
              'OBJLIB_DEPS': '*',
              'LIB_DEPS': '*',
              'PUBLIC_INCLUDES': '*',
          },
          'pargs': 0,
      },
      'draco_generate_emscripten_glue': {
          'kwargs': {
              'INPUT_IDL': '*',
              'OUTPUT_PATH': '*',
          },
          'pargs': 0,
      },
      'draco_get_required_emscripten_flags': {
          'kwargs': {
              'FLAG_LIST_VAR_COMPILER': '*',
              'FLAG_LIST_VAR_LINKER': '*',
          },
          'pargs': 0,
      },
      'draco_option': {
          'kwargs': {
              'NAME': '*',
              'HELPSTRING': '*',
              'VALUE': '*',
          },
          'pargs': 0,
      },
      'list': {
          'kwargs': {
              'APPEND': '*',
              'FILTER': '*',
              'FIND': '*',
              'GET': '*',
              'INSERT': '*',
              'JOIN': '*',
              'LENGTH': '*',
              'POP_BACK': '*',
              'POP_FRONT': '*',
              'PREPEND': '*',
              'REMOVE_DUPLICATES': '*',
              'REMOVE_ITEM': '*',
              'REVERSE': '*',
              'SORT': '*',
              'SUBLIST': '*',
              'TRANSFORM': '*',
          },
      },
      'protobuf_generate': {
        'kwargs': {
            'IMPORT_DIRS': '*',
            'LANGUAGE': '*',
            'OUT_VAR': '*',
            'PROTOC_OUT_DIR': '*',
            'PROTOS': '*',
        },
      },
  }

with section('format'):

  line_width = 80

  tab_size = 2

  separate_ctrl_name_with_space = False

  separate_fn_name_with_space = False

  dangle_parens = False

  enable_sort = False

  line_ending = 'unix'

  command_case = 'canonical'

  keyword_case = 'upper'
