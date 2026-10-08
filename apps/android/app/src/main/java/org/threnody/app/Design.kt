package org.threnody.app

import android.content.Context
import android.graphics.Typeface
import android.view.View
import android.view.ViewConfiguration

/**
 * Design tokens for consistent spacing, typography, and motion.
 * All values in dp/sp unless noted.
 */
object Design {
    /** Base spacing unit (4dp). All spacing should be multiples of this. */
    const val SPACE = 4

    /** Spacing scale */
    val xs = SPACE * 1   // 4dp
    val sm = SPACE * 2   // 8dp
    val md = SPACE * 3   // 12dp
    val lg = SPACE * 4   // 16dp
    val xl = SPACE * 6   // 24dp
    val xxl = SPACE * 8  // 32dp

    /** Component-specific spacing */
    val screenPadding = lg
    val cardPadding = md
    val buttonPaddingV = sm
    val buttonPaddingH = lg
    val listItemPaddingV = md
    val listItemPaddingH = lg
    val fabMargin = lg

    /** Border radius scale */
    val radiusXs = 4f
    val radiusSm = 8f
    val radiusMd = 12f
    val radiusLg = 16f
    val radiusXl = 24f
    val radiusFull = 9999f

    /** Elevation (dp) */
    val elevationCard = 1f
    val elevationBottomSheet = 8f
    val elevationModal = 16f

    /** Typography scale (sp) */
    val typeDisplay = 32f
    val typeHeadline = 24f
    val typeTitle = 20f
    val typeSubtitle = 16f
    val typeBody = 15f
    val typeBodySmall = 13f
    val typeCaption = 12f
    val typeButton = 14f

    /** Text appearance style resource IDs (for setTextAppearance) */
    val styleHeadlineLarge = R.style.Threnody_TextAppearance_HeadlineLarge
    val styleHeadlineMedium = R.style.Threnody_TextAppearance_HeadlineMedium
    val styleHeadlineSmall = R.style.Threnody_TextAppearance_HeadlineSmall
    val styleTitleLarge = R.style.Threnody_TextAppearance_TitleLarge
    val styleTitleMedium = R.style.Threnody_TextAppearance_TitleMedium
    val styleTitleSmall = R.style.Threnody_TextAppearance_TitleSmall
    val styleBodyLarge = R.style.Threnody_TextAppearance_BodyLarge
    val styleBodyMedium = R.style.Threnody_TextAppearance_BodyMedium
    val styleBodySmall = R.style.Threnody_TextAppearance_BodySmall
    val styleLabelLarge = R.style.Threnody_TextAppearance_LabelLarge
    val styleLabelMedium = R.style.Threnody_TextAppearance_LabelMedium
    val styleLabelSmall = R.style.Threnody_TextAppearance_LabelSmall

    /** Font weights */
    val weightRegular = Typeface.NORMAL
    val weightMedium = Typeface.NORMAL // DeviceDefault doesn't expose medium
    val weightBold = Typeface.BOLD

    /** Line heights (multiplier of font size) */
    val lineHeightTight = 1.2f
    val lineHeightNormal = 1.5f
    val lineHeightRelaxed = 1.6f

    /** Motion durations (ms) */
    val durationFast = 150
    val durationNormal = 250
    val durationSlow = 350

    /** Easing curves */
    val easingStandard = android.view.animation.PathInterpolator(0.4f, 0.0f, 0.2f, 1.0f)
    val easingDecelerate = android.view.animation.DecelerateInterpolator()
    val easingAccelerate = android.view.animation.AccelerateInterpolator()

    /** Touch target minimum (dp) - Material recommends 48dp */
    val touchTargetMin = 48

    /** Haptic feedback */
    fun lightHaptic(view: View) {
        view.performHapticFeedback(android.view.HapticFeedbackConstants.LONG_PRESS, android.view.HapticFeedbackConstants.FLAG_IGNORE_GLOBAL_SETTING)
    }

    fun mediumHaptic(view: View) {
        view.performHapticFeedback(android.view.HapticFeedbackConstants.VIRTUAL_KEY, android.view.HapticFeedbackConstants.FLAG_IGNORE_GLOBAL_SETTING)
    }

    fun successHaptic(view: View) {
        if (android.os.Build.VERSION.SDK_INT >= 29) {
            view.performHapticFeedback(android.view.HapticFeedbackConstants.CONTEXT_CLICK, android.view.HapticFeedbackConstants.FLAG_IGNORE_GLOBAL_SETTING)
        } else {
            mediumHaptic(view)
        }
    }

    fun errorHaptic(view: View) {
        if (android.os.Build.VERSION.SDK_INT >= 29) {
            view.performHapticFeedback(android.view.HapticFeedbackConstants.KEYBOARD_TAP, android.view.HapticFeedbackConstants.FLAG_IGNORE_GLOBAL_SETTING)
        } else {
            lightHaptic(view)
        }
    }

    /** Apply consistent touch target sizing */
    fun ensureTouchTarget(view: View, minSize: Int = touchTargetMin) {
        view.setMinimumWidth(view.context.dp(minSize))
        view.setMinimumHeight(view.context.dp(minSize))
    }

    /** System tap timeout for double-tap detection */
    val tapTimeout = ViewConfiguration.getTapTimeout()

    /** Long press timeout */
    val longPressTimeout = ViewConfiguration.getLongPressTimeout()
}