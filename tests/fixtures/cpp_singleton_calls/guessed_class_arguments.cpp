// A guessed class argument decides nothing; `&` of an element is a pointer.
// R15-1: an out-of-tree `Constants::KEY` leaves the literal to rank.
class R15Str {};
struct R15Want {
    void SetParam(const R15Str &key, int value);
    void SetParam(const R15Str &key, bool value);
};
void r15_guess(R15Want *w) { w->SetParam(Constants::KEY, 1); }

// R15-2: `&buf[0]` of a reference to a pointer is a pointer.
void R15Write(const int *data, int n);
void R15Write(int value, int n);
void r15_subscript(int *&buf) { R15Write(&buf[0], 4); }
