struct Box {
    char *field;
    Box(char *value) : field(value) {}
};
struct Braced {
    char *field;
    Braced(char *value) : field{value} {}
};
struct Shadow {
    char *field;
    Shadow(char *field) : field(field) {}
};
#define MEMBER_INIT(v) field(v)
struct Macro {
    char *field;
    Macro(char *value) : MEMBER_INIT(value) {}
};
char *identity(char *value) { return value; }
struct Nested {
    char *field;
    Nested(char *value) : field(identity(value)) {}
};
struct Function {
    void (*field)(void);
    Function() : field(Later) {}
    static void Later() {}
};
