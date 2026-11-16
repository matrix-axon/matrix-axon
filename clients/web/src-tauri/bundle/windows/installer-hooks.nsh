; Included by Tauri's NSIS template (bundle.windows.nsis.installerHooks)
; before the template builds its pages, so these defines configure the
; stock MUI2 finish page.

; The template repurposes the finish page's "show readme" checkbox as
; "Create desktop shortcut"; leave it unchecked unless the user opts in.
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

; Link to the project documentation on the finish page.
!define MUI_FINISHPAGE_LINK "Learn more about Axon at matrix-axon.github.io/matrix-axon"
!define MUI_FINISHPAGE_LINK_LOCATION "https://matrix-axon.github.io/matrix-axon/"
; Set the link colour explicitly: left to the MUI2 default it rendered
; white-on-white on the finish page. Hex RRGGBB, as
; NSIS SetCtlColors expects: a standard link blue.
!define MUI_FINISHPAGE_LINK_COLOR "0000EE"
