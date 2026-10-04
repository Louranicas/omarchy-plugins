// Isolated joined fixture only: matched edges to synthetic Session intents; no compositor effects.
#include "transport.hpp"
#include <memory>
#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/managers/input/InputManager.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopManager.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <array>
#include <cstring>
#include <cstdlib>
#include <stdexcept>
#include <sstream>
#include <algorithm>
#include <hyprland/src/managers/KeybindManager.hpp>

namespace {
HANDLE owner;
std::unique_ptr<join_fixture::Sender> sender;
bool baselineHeld=false;
void exportEdge(const char* edge,bool held=false);
std::vector<CHyprSignalListener> listeners;
SP<SHyprCtlCommand> stats,direct;
SP<SKeybind> ownBind, ownRelease;
unsigned releaseMatched=0; bool activePress=false; bool eligible=false;
std::string mode;
bool bindingPending=false, nestedBinding=false;
unsigned bindingMatched=0,bindingRefused=0;
struct Device { WP<IKeyboard> keyboard; std::array<bool, 2> held{}; };
std::vector<Device> devices;
struct Pending { unsigned device=0, code=0, time=0, state=0; bool valid=false; } pending;
unsigned initial=0, raw=0, global=0, matched=0, unknown=0, stale=0, fresh=0, repeats=0, released=0, cancelled=0, reloaded=0, removed=0, keymaps=0, overlap=0;
bool poisoned=false, armed=false, initialized=false;
uint64_t idle=0;
bool supported(unsigned code) { return code==1 || code==2; }
void invalidate() { if(sender)sender->stop();baselineHeld=false;activePress=false; poisoned=true; pending.valid=false; bindingPending=false; }
void exportEdge(const char* edge,bool held) {if(g_pInputManager->m_keyboards.size()!=1||devices.size()!=1||devices[0].keyboard.lock()!=g_pInputManager->m_keyboards[0]){invalidate();return;}if(!sender||!sender->edge(edge,held))invalidate();}
std::string report() {
 std::ostringstream s;
 s << "{\"release_matched\":" << releaseMatched << ",\"binding_matched\":" << bindingMatched << ",\"binding_refused\":" << bindingRefused << ",\"devices\":" << devices.size() << ",\"initial_held\":" << initial << ",\"raw\":" << raw << ",\"global\":" << global << ",\"matched\":" << matched << ",\"unknown\":" << unknown << ",\"stale\":" << stale << ",\"fresh\":" << fresh << ",\"repeats\":" << repeats << ",\"released\":" << released << ",\"cancelled\":" << cancelled << ",\"reloaded\":" << reloaded << ",\"removed\":" << removed << ",\"keymaps\":" << keymaps << ",\"overlap\":" << overlap << ",\"poisoned\":" << (poisoned?"true":"false") << ",\"effect_authority\":false}";
 return s.str();
}
}
APICALL EXPORT std::string PLUGIN_API_VERSION() { return HYPRLAND_API_VERSION; }
APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
 const char* isolated=std::getenv("OMARCHY_INPUT_OBSERVER_ISOLATED");
 if(!isolated || std::strcmp(isolated,"yes")!=0) throw std::runtime_error("observer isolated fixture only");
 if (std::strcmp(__hyprland_api_get_hash(),__hyprland_api_get_client_hash())!=0 || HyprlandAPI::getHyprlandVersion(handle).hash!="efb50993780079460b0cbed1363e2166a2de1d9f") throw std::runtime_error("observer ABI mismatch");
 owner=handle;releaseMatched=0;activePress=false;eligible=false;nestedBinding=false;mode=std::getenv("OBSERVER_CASE")?std::getenv("OBSERVER_CASE"):"matched";bindingPending=false;bindingMatched=bindingRefused=0;
 initial=raw=global=matched=unknown=stale=fresh=repeats=released=cancelled=reloaded=removed=keymaps=overlap=0;poisoned=armed=initialized=false;pending={};idle=0;
 if(g_pInputManager->m_keyboards.size()!=1) throw std::runtime_error("observer census out of bounds");
 devices.reserve(8);
 for(const auto& keyboard:g_pInputManager->m_keyboards) {
  Device d; d.keyboard=keyboard;
  for(unsigned code=1;code<=2;++code){ d.held[code-1]=keyboard->getPressed(code); if(d.held[code-1])++initial; }
  devices.push_back(d); const unsigned id=devices.size()-1;
  listeners.push_back(keyboard->m_keyboardEvents.key.listen([id](const IKeyboard::SKeyEvent& event){
   if(!supported(event.keycode))return;
   if(!armed){invalidate();return;}
   if(raw>=128){invalidate();return;} ++raw;
   if(pending.valid||bindingPending){++stale;invalidate();}
   auto& d=devices[id]; auto& held=d.held[event.keycode-1]; const bool down=event.state==WL_KEYBOARD_KEY_STATE_PRESSED;
   if(down){if(held)++repeats;else if(!poisoned)++fresh;for(unsigned n=0;n<devices.size();++n)if(n!=id&&devices[n].held[event.keycode-1])++overlap;}else if(held)++released;
   eligible=down?!held:(activePress||baselineHeld); held=down; pending={id,event.keycode,event.timeMs,static_cast<unsigned>(event.state),!poisoned};
   if(idle)g_pEventLoopManager->removeDoLater(idle);
   idle=g_pEventLoopManager->doLater([]{idle=0;if(bindingPending)invalidate();bindingPending=false;if(pending.valid){pending.valid=false;++stale;invalidate();}});
  }));
  listeners.push_back(keyboard->m_events.destroy.listen([]{++removed;invalidate();}));
  listeners.push_back(keyboard->m_keyboardEvents.keymap.listen([]{++keymaps;invalidate();}));
 }
 // Known test cancellation registered before observer; never exports an action.
 listeners.push_back(Event::bus()->m_events.input.keyboard.key.listen([](const IKeyboard::SKeyEvent& e, Event::SCallbackInfo& info){if(e.keycode==2)info.cancelled=true;}));
 if(mode=="reentrant")listeners.push_back(Event::bus()->m_events.input.keyboard.key.listen([](const IKeyboard::SKeyEvent& e,Event::SCallbackInfo&){static bool inside=false;if(!inside){inside=true;Event::SCallbackInfo nested;Event::bus()->m_events.input.keyboard.key.emit(e,nested);inside=false;}}));
 listeners.push_back(Event::bus()->m_events.input.keyboard.key.listen([](const IKeyboard::SKeyEvent& e, const Event::SCallbackInfo& info){
  if(!supported(e.keycode))return;
  if(global>=128){invalidate();return;}++global;
  if(!pending.valid){++unknown;invalidate();return;}
  if(pending.code!=e.keycode||pending.time!=e.timeMs||pending.state!=static_cast<unsigned>(e.state)){++stale;invalidate();return;}
  pending.valid=false;++matched;if(info.cancelled){++cancelled;invalidate();}bindingPending=!info.cancelled&&!poisoned;
 }));
 if(mode=="later-cancel")listeners.push_back(Event::bus()->m_events.input.keyboard.key.listen([](const IKeyboard::SKeyEvent&,Event::SCallbackInfo& info){info.cancelled=true;}));
 if(!HyprlandAPI::addDispatcherV2(owner,"observerdiag",[](std::string){if(!poisoned&&bindingPending&&eligible&&((pending.state==WL_KEYBOARD_KEY_STATE_PRESSED&&g_pKeybindManager->m_currentKeybind==ownBind)||(pending.state==WL_KEYBOARD_KEY_STATE_RELEASED&&(activePress||baselineHeld)&&g_pKeybindManager->m_currentKeybind==ownRelease))){bindingPending=false;if(pending.state==WL_KEYBOARD_KEY_STATE_PRESSED){activePress=true;++bindingMatched;exportEdge("down");}else{activePress=false;if(!baselineHeld)++releaseMatched;baselineHeld=false;exportEdge("up");}if(mode=="binding-reentrant"&&!nestedBinding){nestedBinding=true;g_pKeybindManager->m_dispatchers.at("observerdiag")("");nestedBinding=false;}}else{++bindingRefused;}return SDispatchResult{};}))throw std::runtime_error("counter dispatcher unavailable");

 listeners.push_back(Event::bus()->m_events.config.preReload.listen([]{++reloaded;if(initialized)invalidate();}));
 listeners.push_back(Event::bus()->m_events.config.reloaded.listen([]{
  if(initialized){invalidate();return;} initialized=true;
  if(poisoned||reloaded!=1||g_pInputManager->m_keyboards.size()!=devices.size()){invalidate();return;}
  initial=0;
  for(unsigned n=0;n<devices.size();++n){auto keyboard=devices[n].keyboard.lock();if(!keyboard||keyboard!=g_pInputManager->m_keyboards[n]){invalidate();return;}for(unsigned code=1;code<=2;++code){devices[n].held[code-1]=keyboard->getPressed(code);if(devices[n].held[code-1])++initial;}}
 SKeybind bind;bind.keycode=9;bind.handler="observerdiag";bind.ignoreMods=true;ownBind=g_pKeybindManager->addKeybind(bind);bind.release=true;ownRelease=g_pKeybindManager->addKeybind(bind);
  // Trusted disposable harness admission, not a public registration endpoint.
  int cfg=open("/sandbox/join/admission",O_RDONLY|O_CLOEXEC|O_NOFOLLOW);
  if(cfg<0){invalidate();return;}
  try {
   auto data=join_fixture::readBounded(cfg,2048);close(cfg);cfg=-1;
   std::istringstream lines(data);std::string pid,start,prefix,extra;
   if(!std::getline(lines,pid)||!std::getline(lines,start)||!std::getline(lines,prefix)||std::getline(lines,extra))throw std::runtime_error("admission format");
   size_t used=0;auto peer=std::stol(pid,&used);if(used!=pid.size()||peer<=0||peer>2147483647)throw std::runtime_error("pid");auto ticks=std::stoull(start,&used);if(used!=start.size()||ticks==0)throw std::runtime_error("start");
   int fd=socket(AF_UNIX,SOCK_STREAM|SOCK_NONBLOCK|SOCK_CLOEXEC,0);if(fd<0)throw std::runtime_error("socket");
   sockaddr_un address{};address.sun_family=AF_UNIX;strcpy(address.sun_path,"/sandbox/join/modal.sock");
   if(connect(fd,reinterpret_cast<sockaddr*>(&address),sizeof address)){close(fd);throw std::runtime_error("connect");}
   sender=std::make_unique<join_fixture::Sender>(fd,pid_t(peer),ticks,prefix);
   baselineHeld=devices[0].held[0];armed=true;exportEdge("ready",baselineHeld);
  }catch(...){if(cfg>=0)close(cfg);invalidate();}
 }));
 stats=HyprlandAPI::registerHyprCtlCommand(owner,{.name="observerstats",.exact=true,.fn=[](eHyprCtlOutputFormat,std::string){return report();}});
 direct=HyprlandAPI::registerHyprCtlCommand(owner,{.name="observerdirect",.exact=true,.fn=[](eHyprCtlOutputFormat,std::string){g_pKeybindManager->m_dispatchers.at("observerdiag")("");return report();}});
 if(!stats||!direct)throw std::runtime_error("observer stats registration failed");
 return {"isolated-input-observer","Bounded counters; no production action path","Omarchy fixture","0.0.1"};
}
APICALL EXPORT void PLUGIN_EXIT() {
 if(idle){g_pEventLoopManager->removeDoLater(idle);idle=0;}
 if(direct){HyprlandAPI::unregisterHyprCtlCommand(owner,direct);direct.reset();}
 invalidate();sender.reset();listeners.clear();devices.clear();if(ownRelease){std::erase(g_pKeybindManager->m_keybinds,ownRelease);ownRelease.reset();}if(ownBind){std::erase(g_pKeybindManager->m_keybinds,ownBind);ownBind.reset();}HyprlandAPI::removeDispatcher(owner,"observerdiag");if(stats){HyprlandAPI::unregisterHyprCtlCommand(owner,stats);stats.reset();}
}
