use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{Arc, Mutex, mpsc};

use pam_client::Context;
use vigil_core::AuthEvent;

use super::Bridge;

const PAM_CONV: c_int = 5;
const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_MAX_NUM_MSG: usize = 32;

const ECHO_OFF: c_int = 1;
const ECHO_ON: c_int = 2;
const ERROR_MSG: c_int = 3;
const TEXT_INFO: c_int = 4;
const BINARY_PROMPT: c_int = 7;

#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}

#[repr(C)]
struct Response {
    text: *mut c_char,
    retcode: c_int,
}

type Converse =
    unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int;

#[repr(C)]
struct Conversation {
    converse: Option<Converse>,
    appdata: *mut c_void,
}

type Sink = Box<dyn Fn(AuthEvent)>;

struct Module {
    context: Context<Bridge<Sink>>,
    events: Arc<Mutex<Vec<AuthEvent>>>,
    field: Option<mpsc::Sender<String>>,
}

impl Module {
    fn load() -> Option<Self> {
        let events: Arc<Mutex<Vec<AuthEvent>>> = Arc::default();
        let sink = events.clone();
        let (field, answers) = mpsc::channel();
        let emit: Sink = Box::new(move |event| sink.lock().unwrap().push(event));
        match Context::new(
            "vigil-hostile-module",
            Some("nobody"),
            Bridge::new(emit, answers),
        ) {
            Ok(context) => Some(Self {
                context,
                events,
                field: Some(field),
            }),
            Err(error) if std::env::var_os("CI").is_none() => {
                eprintln!("SKIP: libpam would not start here ({error})");
                None
            }
            Err(error) => panic!("pam_start: {error}"),
        }
    }

    fn typed(&self, text: &str) {
        self.field.as_ref().unwrap().send(text.into()).unwrap();
    }

    fn events(&self) -> Vec<AuthEvent> {
        std::mem::take(&mut self.events.lock().unwrap())
    }

    fn conversation(&self) -> &Conversation {
        let item = self.context.get_item(PAM_CONV).expect("PAM_CONV");
        // SAFETY: libpam hands back the pam_conv it copied at pam_start.
        unsafe { &*item.cast::<Conversation>() }
    }

    fn raw(&self, count: c_int, messages: *mut *const Message, out: *mut *mut Response) -> c_int {
        let conversation = self.conversation();
        let converse = conversation.converse.expect("a conversation function");
        // SAFETY: called the way a PAM module calls the application's function.
        unsafe { converse(count, messages, out, conversation.appdata) }
    }

    fn ask(&self, messages: &[(c_int, Option<&CStr>)]) -> Result<Vec<Option<CString>>, c_int> {
        let messages: Vec<Message> = messages
            .iter()
            .map(|(style, text)| Message {
                style: *style,
                text: text.map_or(ptr::null(), CStr::as_ptr),
            })
            .collect();
        let mut pointers: Vec<*const Message> = messages.iter().map(ptr::from_ref).collect();
        let untouched = ptr::dangling_mut::<Response>();
        let mut out = untouched;
        let code = self.raw(messages.len() as c_int, pointers.as_mut_ptr(), &mut out);
        if code != PAM_SUCCESS {
            assert_eq!(out, untouched, "a failed conversation handed memory back");
            return Err(code);
        }
        // SAFETY: on success the module owns the array and every answer in
        // it, and releases each with free(), as libpam's modules do.
        let answers = unsafe {
            let answers = std::slice::from_raw_parts(out, messages.len())
                .iter()
                .map(|response| {
                    assert_eq!(response.retcode, 0);
                    (!response.text.is_null()).then(|| {
                        let text = CStr::from_ptr(response.text).to_owned();
                        libc::free(response.text.cast());
                        text
                    })
                })
                .collect();
            libc::free(out.cast());
            answers
        };
        Ok(answers)
    }
}

fn text(text: &str) -> CString {
    CString::new(text).unwrap()
}

fn prompt(text: &str, secret: bool) -> AuthEvent {
    AuthEvent::Prompt {
        text: text.into(),
        secret,
    }
}

#[test]
fn each_answer_is_the_modules_own_allocation() {
    let Some(module) = Module::load() else { return };
    module.typed("hunter2");
    module.typed("123456");
    let (password, note, code) = (text("Password: "), text("note"), text("Code: "));
    let answers = module
        .ask(&[
            (ECHO_OFF, Some(&*password)),
            (TEXT_INFO, Some(&*note)),
            (ECHO_ON, Some(&*code)),
        ])
        .unwrap();
    assert_eq!(answers, [Some(text("hunter2")), None, Some(text("123456"))]);
    assert_eq!(
        module.events(),
        [
            prompt("Password: ", true),
            AuthEvent::Info("note".into()),
            prompt("Code: ", false)
        ]
    );
}

#[test]
fn the_largest_message_count_libpam_allows_is_answered() {
    let Some(module) = Module::load() else { return };
    let question = text("Password: ");
    for index in 0..PAM_MAX_NUM_MSG {
        module.typed(&format!("answer {index}"));
    }
    let answers = module
        .ask(&[(ECHO_OFF, Some(&*question)); PAM_MAX_NUM_MSG])
        .unwrap();
    for (index, answer) in answers.iter().enumerate() {
        assert_eq!(*answer, Some(text(&format!("answer {index}"))));
    }
}

#[test]
fn a_count_no_module_may_send_is_refused() {
    let Some(module) = Module::load() else { return };
    let question = text("Password: ");
    let message = Message {
        style: ECHO_OFF,
        text: question.as_ptr(),
    };
    let mut pointers = [ptr::from_ref(&message)];
    for count in [0, -1, c_int::MIN] {
        let untouched = ptr::dangling_mut::<Response>();
        let mut out = untouched;
        assert_eq!(
            module.raw(count, pointers.as_mut_ptr(), &mut out),
            PAM_BUF_ERR,
            "count {count}"
        );
        assert_eq!(out, untouched, "count {count} handed memory back");
    }
    assert_eq!(module.events(), [], "a refused call still asked the user");
}

#[test]
fn null_arguments_are_refused() {
    let Some(module) = Module::load() else { return };
    let question = text("Password: ");
    let message = Message {
        style: ECHO_OFF,
        text: question.as_ptr(),
    };
    let mut pointers = [ptr::from_ref(&message)];
    let mut out = ptr::null_mut();
    assert_eq!(module.raw(1, ptr::null_mut(), &mut out), PAM_BUF_ERR);
    assert_eq!(
        module.raw(1, pointers.as_mut_ptr(), ptr::null_mut()),
        PAM_BUF_ERR
    );
    assert!(out.is_null());
    assert_eq!(module.events(), []);
}

#[test]
fn a_message_without_text_is_an_empty_question() {
    let Some(module) = Module::load() else { return };
    module.typed("hunter2");
    let answers = module.ask(&[(ECHO_OFF, None), (ERROR_MSG, None)]).unwrap();
    assert_eq!(answers, [Some(text("hunter2")), None]);
    assert_eq!(
        module.events(),
        [prompt("", true), AuthEvent::Error(String::new())]
    );
}

#[test]
fn an_oversized_message_arrives_whole_and_bad_utf8_is_replaced() {
    let Some(module) = Module::load() else { return };
    let mut bytes = vec![b'A'; 1 << 20];
    bytes.extend_from_slice(b"\xff\xfe\xc3");
    let huge = CString::new(bytes).unwrap();
    module.typed("hunter2");
    module.ask(&[(ECHO_OFF, Some(&*huge))]).unwrap();
    let [AuthEvent::Prompt { text, secret: true }] = &module.events()[..] else {
        panic!("expected one hidden prompt");
    };
    assert_eq!(text.len(), (1 << 20) + 3 * '\u{fffd}'.len_utf8());
    assert!(text.ends_with("A\u{fffd}\u{fffd}\u{fffd}"));
}

#[test]
fn an_unknown_style_ends_the_conversation_and_frees_what_was_answered() {
    let Some(module) = Module::load() else { return };
    let question = text("Password: ");
    for style in [0, -1, 6, 99, c_int::MAX, BINARY_PROMPT] {
        module.typed("hunter2");
        let outcome = module.ask(&[(ECHO_OFF, Some(&*question)), (style, None)]);
        assert_eq!(outcome, Err(PAM_CONV_ERR), "style {style}");
        assert_eq!(module.events(), [prompt("Password: ", true)]);
    }
}

#[test]
fn an_answer_with_a_nul_fails_the_conversation_instead_of_being_cut_short() {
    let Some(module) = Module::load() else { return };
    let question = text("Password: ");
    module.typed("hun\0ter2");
    assert_eq!(
        module.ask(&[(ECHO_OFF, Some(&*question))]),
        Err(PAM_CONV_ERR)
    );
}

#[test]
fn a_field_that_went_away_fails_the_conversation() {
    let Some(mut module) = Module::load() else {
        return;
    };
    module.field = None;
    let question = text("Password: ");
    assert_eq!(
        module.ask(&[(ECHO_OFF, Some(&*question))]),
        Err(PAM_CONV_ERR)
    );
}
