// A default argument on a prototype its definition does not reunite with:
// the out-of-tree `std::string` is spelled `string` under `using namespace`.
namespace text {
void Trim(const std::string &s, char c = ' ');
}
