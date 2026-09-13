// Included by both files: each unit's `static` functions stay its own.
struct SharedArg {
    int x;
};

static void SharedTag(int x) {}

static void SharedDeclared(SharedArg a);
