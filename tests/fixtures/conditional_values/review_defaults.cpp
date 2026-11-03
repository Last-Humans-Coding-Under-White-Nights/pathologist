using DefaultsCallback = void (*)();
void alpha();
void beta();
void unrelated();
DefaultsCallback defaults_full_cell = beta;
DefaultsCallback defaults_gap_cell = beta;
DefaultsCallback defaults_reordered_cell = beta;
DefaultsCallback defaults_unresolved_cell = beta;
struct DefaultsFull { DefaultsCallback f = ((defaults_full_cell = unrelated), beta); };
struct DefaultsGap { DefaultsCallback f = ((defaults_gap_cell = alpha), beta); };
struct DefaultsReordered { DefaultsCallback f = ((defaults_reordered_cell = unrelated), beta); };
struct DefaultsUnresolved { DefaultsCallback f = ((defaults_unresolved_cell = alpha), beta); };
// C99 array designators are accepted here as a C++ extension (Clang).
DefaultsFull defaults_full[] = {[0] = {alpha}, [1] = {beta}};
DefaultsGap defaults_gap[] = {[1] = {beta}};
DefaultsReordered defaults_reordered[] = {[1] = {beta}, [0] = {alpha}};
// A written bound that the analyzer cannot compute remains conservative.
DefaultsUnresolved defaults_unresolved[sizeof(char[2])] = {[0] = {beta}};
void defaults_unresolved_check() { defaults_unresolved_cell(); }
void defaults_full_check() { defaults_full_cell(); }
void defaults_gap_check() { defaults_gap_cell(); }
void defaults_reordered_check() { defaults_reordered_cell(); }

DefaultsCallback defaults_explicit_a = alpha;
DefaultsCallback defaults_explicit_b = beta;
struct DefaultsExplicitMember {
    DefaultsCallback &get(DefaultsCallback &v) { return v; }
    DefaultsCallback &second(int, DefaultsCallback &v) { return v; }
};
DefaultsExplicitMember defaults_explicit_member;
DefaultsCallback *defaults_explicit_pointer = &defaults_explicit_member.get(defaults_explicit_a);
DefaultsCallback *defaults_second_pointer = &defaults_explicit_member.second(0, defaults_explicit_b);
void defaults_member_explicit_check() { (*defaults_explicit_pointer)(); }
void defaults_member_second_check() { (*defaults_second_pointer)(); }
