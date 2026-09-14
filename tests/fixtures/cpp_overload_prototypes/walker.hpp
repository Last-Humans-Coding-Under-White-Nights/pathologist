// A static member template beside an instance overload of the same name.
typedef void (*Callback)();

class Walker {
public:
    template <typename T>
    static bool Walk(Callback start, T callback)
    {
        start();
        return callback != 0;
    }

    template <typename T>
    bool Walk(T callback)
    {
        return Walk(root_, callback);
    }

private:
    Callback root_;
};
