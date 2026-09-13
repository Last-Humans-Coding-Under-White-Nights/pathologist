// An unrelated class template with the same qualified name as `Listener`.
template <typename... T> class Listener {
public:
    void Fire(T const... args) const;
};

template <typename... T> void Listener<T...>::Fire(T const... args) const {}
