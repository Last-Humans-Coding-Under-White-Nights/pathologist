void ExternalCopySide() {}
struct External {
    External(const External &other) { ExternalCopySide(); }
};
