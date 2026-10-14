/* HDF's compiler selection, with observable branch declarations. */
#if defined(__GNUC__) || defined(__clang__)
void gnu_section(void) {}
#elif defined(__ICCARM__)
void iar_section(void) {}
#else
#error "No support section begin and section end!"
#endif
