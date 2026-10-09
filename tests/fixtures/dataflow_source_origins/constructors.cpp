struct Box {
    char *saved;
    Box(char *p) { saved = p; }
};
char *identity(char *p) { return p; }
#define STACK(v) Box macro_local(v)
#define HEAP(v) new Box(v)
void build(char *input) {
    Box local(input);
    Box *heap = new Box(input);
    Box nested_local{identity(input)};
    Box *nested_heap = new Box(identity(input));
    STACK(input);
    Box *macro_heap = HEAP(input);
    Box brace_local{input};
    Box *brace_heap = new Box{input};
}
struct Base { Base(char *p) {} };
struct Member { Member(char *p) {} };
struct Derived : Base {
    Member member;
    Derived(char *p) : Base(p), member(p) {}
};
