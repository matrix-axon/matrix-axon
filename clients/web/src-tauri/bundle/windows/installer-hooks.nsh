; Included by Tauri's NSIS template (bundle.windows.nsis.installerHooks)
; before the template builds its pages, so these defines configure the
; stock MUI2 finish page.

; The template repurposes the finish page's "show readme" checkbox as
; "Create desktop shortcut"; leave it unchecked unless the user opts in.
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

; Link to the project documentation on the finish page. The link control is
; a single line about 290px wide; longer text wraps and the extra line is
; clipped, so keep the label short.
!define MUI_FINISHPAGE_LINK "Click here to learn about Axon."
!define MUI_FINISHPAGE_LINK_LOCATION "https://matrix-axon.github.io/matrix-axon/"
; Set the link colour explicitly (hex RRGGBB, a standard link blue) rather
; than relying on the MUI2 default.
!define MUI_FINISHPAGE_LINK_COLOR "0000EE"
