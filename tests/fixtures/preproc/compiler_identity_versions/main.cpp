#if defined(EXPECTED_MSC_VER)
#if _MSC_VER == EXPECTED_MSC_VER
void SELECTED_FUNCTION(void) {}
#else
void wrong_msvc_identity(void) {}
#endif
#else
#if __GNUC__ == EXPECTED_GNU_MAJOR && __GNUC_MINOR__ == EXPECTED_GNU_MINOR && __GNUC_PATCHLEVEL__ == EXPECTED_GNU_PATCH && __GNUG__ == EXPECTED_GNU_MAJOR
void SELECTED_FUNCTION(void) {}
#else
void wrong_gnu_identity(void) {}
#endif
#endif
